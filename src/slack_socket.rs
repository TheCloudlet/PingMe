use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_WATCHDOG: Duration = Duration::from_secs(30);
const DEFAULT_MIN_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Slack Socket Mode connection. Callers see envelopes and a state enum;
/// URL minting, overlap refresh, backoff, acks, pongs, and dead-socket
/// detection stay inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Down,
    Rebuilding,
    Up,
}

pub struct SlackSocketMode {
    envelopes: mpsc::UnboundedReceiver<Value>,
    state: watch::Receiver<ConnectionState>,
    epoch: watch::Receiver<f64>,
    driver: JoinHandle<()>,
}

impl SlackSocketMode {
    pub fn connect(http: reqwest::Client, app_token: String) -> Self {
        start_with(RealDialer { http, app_token }, SocketConfig::default())
    }

    pub async fn recv(&mut self) -> Option<Value> {
        self.envelopes.recv().await
    }

    pub fn state(&self) -> ConnectionState {
        *self.state.borrow()
    }

    /// Time the current live generation became Up. Unchanged during overlap.
    pub fn epoch(&self) -> f64 {
        *self.epoch.borrow()
    }
}

impl Drop for SlackSocketMode {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

struct SocketConfig {
    watchdog: Duration,
    min_backoff: Duration,
    max_backoff: Duration,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            watchdog: DEFAULT_WATCHDOG,
            min_backoff: DEFAULT_MIN_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

enum Frame {
    Text(String),
    Ping(Vec<u8>),
    Close,
}

#[derive(Debug)]
enum Outgoing {
    Text(String),
    Pong(Vec<u8>),
    Close,
}

struct SocketEnds {
    incoming: mpsc::UnboundedReceiver<Result<Frame, String>>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
}

trait Dialer: Send + Sync + 'static {
    fn dial(&self) -> impl Future<Output = Result<SocketEnds, String>> + Send;
}

struct RealDialer {
    http: reqwest::Client,
    app_token: String,
}

impl Dialer for RealDialer {
    fn dial(&self) -> impl Future<Output = Result<SocketEnds, String>> + Send {
        let http = self.http.clone();
        let app_token = self.app_token.clone();
        async move {
            let socket_url = open_slack_socket(&http, &app_token).await?;
            let (websocket, _) = tokio_tungstenite::connect_async(socket_url)
                .await
                .map_err(|error| format!("failed to connect Slack Socket Mode: {error}"))?;
            Ok(pump_websocket(websocket))
        }
    }
}

type TungsteniteStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn pump_websocket(websocket: TungsteniteStream) -> SocketEnds {
    let (mut write, mut read) = websocket.split();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                incoming = read.next() => {
                    let frame = match incoming {
                        None => Ok(Frame::Close),
                        Some(Err(error)) => Err(error.to_string()),
                        Some(Ok(Message::Text(text))) => Ok(Frame::Text(text.to_string())),
                        Some(Ok(Message::Ping(payload))) => Ok(Frame::Ping(payload.to_vec())),
                        Some(Ok(Message::Close(_))) => Ok(Frame::Close),
                        Some(Ok(_)) => continue,
                    };
                    let closed = matches!(frame, Ok(Frame::Close) | Err(_));
                    if incoming_tx.send(frame).is_err() || closed {
                        break;
                    }
                }
                outgoing = outgoing_rx.recv() => {
                    let Some(outgoing) = outgoing else {
                        break;
                    };
                    let closing = matches!(outgoing, Outgoing::Close);
                    let message = match outgoing {
                        Outgoing::Text(text) => Message::text(text),
                        Outgoing::Pong(payload) => Message::Pong(payload.into()),
                        Outgoing::Close => Message::Close(None),
                    };
                    if write.send(message).await.is_err() || closing {
                        break;
                    }
                }
            }
        }
    });
    SocketEnds {
        incoming: incoming_rx,
        outgoing: outgoing_tx,
    }
}

async fn open_slack_socket(http: &reqwest::Client, app_token: &str) -> Result<String, String> {
    let response: Value = http
        .post("https://slack.com/api/apps.connections.open")
        .bearer_auth(app_token)
        .send()
        .await
        .map_err(|error| format!("Slack apps.connections.open failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("Slack apps.connections.open returned invalid JSON: {error}"))?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "Slack API error: {}",
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
        ));
    }
    response
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Slack response omitted WebSocket URL".to_owned())
}

enum ConnEvent {
    Overlap(u64),
    Healthy(u64),
    Died(u64),
}

struct LiveConn {
    id: u64,
    task: JoinHandle<()>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
}

impl LiveConn {
    fn close(self) {
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

impl Drop for LiveConn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Internal machine. Illegal combinations cannot be constructed:
/// overlap with zero sockets, dual while rebuilding, two live sockets
/// while claiming Down.
enum Machine {
    Down { delay: Duration },
    Rebuilding { delay: Duration },
    Live { conn: LiveConn },
    Overlapping { old: LiveConn },
    Dual { old: LiveConn, new: LiveConn },
}

impl Machine {
    fn public(&self) -> ConnectionState {
        match self {
            Self::Down { .. } => ConnectionState::Down,
            Self::Rebuilding { .. } => ConnectionState::Rebuilding,
            Self::Live { .. } | Self::Overlapping { .. } | Self::Dual { .. } => ConnectionState::Up,
        }
    }

    async fn step<D: Dialer>(self, driver: &mut Driver<D>) -> Self {
        match self {
            Self::Down { delay } => driver.connect_disconnected(false, delay).await,
            Self::Rebuilding { delay } => driver.connect_disconnected(true, delay).await,
            Self::Live { conn } => driver.hold_live(conn).await,
            Self::Overlapping { old } => driver.overlap(old).await,
            Self::Dual { old, new } => driver.hold_dual(old, new).await,
        }
    }
}

struct Driver<D> {
    dialer: D,
    config: SocketConfig,
    envelopes: mpsc::UnboundedSender<Value>,
    events_tx: mpsc::UnboundedSender<ConnEvent>,
    events_rx: mpsc::UnboundedReceiver<ConnEvent>,
    epoch: watch::Sender<f64>,
    next_id: u64,
}

fn start_with<D: Dialer>(dialer: D, config: SocketConfig) -> SlackSocketMode {
    let (envelopes_tx, envelopes_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(ConnectionState::Down);
    let (epoch_tx, epoch_rx) = watch::channel(unix_seconds());
    let driver = tokio::spawn(run_driver(dialer, config, envelopes_tx, state_tx, epoch_tx));
    SlackSocketMode {
        envelopes: envelopes_rx,
        state: state_rx,
        epoch: epoch_rx,
        driver,
    }
}

async fn run_driver<D: Dialer>(
    dialer: D,
    config: SocketConfig,
    envelopes: mpsc::UnboundedSender<Value>,
    state: watch::Sender<ConnectionState>,
    epoch: watch::Sender<f64>,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let min_backoff = config.min_backoff;
    let mut driver = Driver {
        dialer,
        config,
        envelopes,
        events_tx,
        events_rx,
        epoch,
        next_id: 1,
    };
    let mut machine = Machine::Down { delay: min_backoff };
    loop {
        let _ = state.send(machine.public());
        machine = machine.step(&mut driver).await;
    }
}

async fn recv_event(events: &mut mpsc::UnboundedReceiver<ConnEvent>) -> ConnEvent {
    match events.recv().await {
        Some(event) => event,
        None => std::future::pending().await,
    }
}

impl<D: Dialer> Driver<D> {
    fn spawn(&mut self, ends: SocketEnds) -> LiveConn {
        let id = self.next_id;
        self.next_id += 1;
        let outgoing = ends.outgoing.clone();
        let task = tokio::spawn(run_connection(
            id,
            ends,
            self.events_tx.clone(),
            self.envelopes.clone(),
            self.config.watchdog,
        ));
        LiveConn { id, task, outgoing }
    }

    fn disconnected(&self, ever_up: bool, delay: Duration) -> Machine {
        if ever_up {
            Machine::Rebuilding { delay }
        } else {
            Machine::Down { delay }
        }
    }

    async fn connect_disconnected(&mut self, ever_up: bool, delay: Duration) -> Machine {
        match self.dialer.dial().await {
            Ok(ends) => {
                let conn = self.spawn(ends);
                let _ = self.epoch.send(unix_seconds());
                Machine::Live { conn }
            }
            Err(error) => {
                eprintln!("Slack Socket Mode connect failed, retrying in {delay:?}: {error}");
                tokio::time::sleep(delay).await;
                self.disconnected(ever_up, (delay * 2).min(self.config.max_backoff))
            }
        }
    }

    async fn hold_live(&mut self, conn: LiveConn) -> Machine {
        loop {
            match recv_event(&mut self.events_rx).await {
                ConnEvent::Overlap(id) if id == conn.id => {
                    return Machine::Overlapping { old: conn };
                }
                ConnEvent::Died(id) if id == conn.id => {
                    return self.disconnected(true, self.config.min_backoff);
                }
                _ => {}
            }
        }
    }

    async fn overlap(&mut self, old: LiveConn) -> Machine {
        let old_id = old.id;
        let min_backoff = self.config.min_backoff;
        let result = {
            let dial = self.dialer.dial();
            tokio::pin!(dial);
            loop {
                tokio::select! {
                    result = &mut dial => break result,
                    event = recv_event(&mut self.events_rx) => {
                        if let ConnEvent::Died(id) = event
                            && id == old_id
                        {
                            return Machine::Rebuilding { delay: min_backoff };
                        }
                    }
                }
            }
        };
        match result {
            Ok(ends) => Machine::Dual {
                old,
                new: self.spawn(ends),
            },
            Err(error) => {
                eprintln!("Slack Socket Mode overlap connect failed: {error}");
                Machine::Live { conn: old }
            }
        }
    }

    async fn hold_dual(&mut self, old: LiveConn, new: LiveConn) -> Machine {
        loop {
            match recv_event(&mut self.events_rx).await {
                ConnEvent::Healthy(id) if id == new.id => {
                    old.close();
                    return Machine::Live { conn: new };
                }
                ConnEvent::Died(id) if id == old.id => return Machine::Live { conn: new },
                ConnEvent::Died(id) if id == new.id => return Machine::Live { conn: old },
                _ => {}
            }
        }
    }
}

async fn run_connection(
    id: u64,
    mut ends: SocketEnds,
    events: mpsc::UnboundedSender<ConnEvent>,
    envelopes: mpsc::UnboundedSender<Value>,
    watchdog: Duration,
) {
    loop {
        match timeout(watchdog, ends.incoming.recv()).await {
            Err(_) => {
                eprintln!("Slack Socket Mode watchdog expired");
                let _ = events.send(ConnEvent::Died(id));
                return;
            }
            Ok(None) | Ok(Some(Ok(Frame::Close))) => {
                let _ = events.send(ConnEvent::Died(id));
                return;
            }
            Ok(Some(Err(error))) => {
                eprintln!("Slack websocket error, reconnecting: {error}");
                let _ = events.send(ConnEvent::Died(id));
                return;
            }
            Ok(Some(Ok(Frame::Ping(payload)))) => {
                if ends.outgoing.send(Outgoing::Pong(payload)).is_err() {
                    let _ = events.send(ConnEvent::Died(id));
                    return;
                }
            }
            Ok(Some(Ok(Frame::Text(text)))) => {
                let Ok(envelope) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(envelope_id) = envelope.get("envelope_id").and_then(Value::as_str)
                    && ends
                        .outgoing
                        .send(Outgoing::Text(
                            serde_json::json!({ "envelope_id": envelope_id }).to_string(),
                        ))
                        .is_err()
                {
                    let _ = events.send(ConnEvent::Died(id));
                    return;
                }
                match envelope.get("type").and_then(Value::as_str) {
                    Some("hello") => {
                        let _ = events.send(ConnEvent::Healthy(id));
                    }
                    Some("disconnect") => {
                        let reason = envelope.get("reason").and_then(Value::as_str).unwrap_or("");
                        if reason == "link_disabled" {
                            eprintln!("Slack Socket Mode disabled");
                            let _ = events.send(ConnEvent::Died(id));
                            return;
                        }
                        if reason == "too_many_websockets" {
                            // Slack is telling us we already hold too many open
                            // connections. Overlapping (dialing another one before
                            // closing this one) would only add to that count and
                            // spins into a reconnect storm that burns the API rate
                            // limit. Drop this connection first and let the normal
                            // backed-off Down/Rebuilding path reconnect.
                            eprintln!("Slack Socket Mode has too many open websockets, backing off");
                            let _ = events.send(ConnEvent::Died(id));
                            return;
                        }
                        eprintln!("Slack requested reconnect ({reason})");
                        let _ = events.send(ConnEvent::Overlap(id));
                    }
                    _ => {
                        if envelopes.send(envelope).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct TestDialer {
        requests: mpsc::UnboundedSender<oneshot::Sender<Result<SocketEnds, String>>>,
    }

    struct TestDials {
        requests: mpsc::UnboundedReceiver<oneshot::Sender<Result<SocketEnds, String>>>,
    }

    struct TestConn {
        to_app: mpsc::UnboundedSender<Result<Frame, String>>,
        from_app: mpsc::UnboundedReceiver<Outgoing>,
    }

    impl Dialer for TestDialer {
        fn dial(&self) -> impl Future<Output = Result<SocketEnds, String>> + Send {
            let requests = self.requests.clone();
            async move {
                let (tx, rx) = oneshot::channel();
                requests
                    .send(tx)
                    .map_err(|_| "test dialer dropped".to_owned())?;
                rx.await.map_err(|_| "test dial cancelled".to_owned())?
            }
        }
    }

    fn test_pair() -> (TestDialer, TestDials) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TestDialer { requests: tx }, TestDials { requests: rx })
    }

    fn test_config() -> SocketConfig {
        SocketConfig {
            watchdog: Duration::from_millis(80),
            min_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
        }
    }

    impl TestDials {
        async fn accept(&mut self) -> TestConn {
            let start = tokio::time::Instant::now();
            loop {
                let reply = timeout(Duration::from_secs(1), self.requests.recv())
                    .await
                    .expect("timed out waiting for dial")
                    .expect("driver stopped dialing");
                let (to_app, incoming) = mpsc::unbounded_channel();
                let (outgoing, from_app) = mpsc::unbounded_channel();
                if reply.send(Ok(SocketEnds { incoming, outgoing })).is_ok() {
                    return TestConn { to_app, from_app };
                }
                if start.elapsed() > Duration::from_secs(1) {
                    panic!("driver cancelled every dial");
                }
            }
        }

        async fn fail(&mut self, error: &str) {
            let reply = timeout(Duration::from_secs(1), self.requests.recv())
                .await
                .expect("timed out waiting for dial")
                .expect("driver stopped dialing");
            if reply.send(Err(error.to_owned())).is_err() {
                panic!("driver dropped dial");
            }
        }
    }

    impl TestConn {
        fn send_text(&self, text: impl Into<String>) {
            self.to_app.send(Ok(Frame::Text(text.into()))).unwrap();
        }

        fn send_hello(&self) {
            self.send_text(r#"{"type":"hello","num_connections":1}"#);
        }

        fn send_event(&self, envelope_id: &str, text: &str) {
            self.send_text(format!(
                r#"{{"type":"events_api","envelope_id":"{envelope_id}","payload":{{"event":{{"type":"message","text":"{text}"}}}}}}"#
            ));
        }

        fn send_disconnect(&self, reason: &str) {
            self.send_text(format!(r#"{{"type":"disconnect","reason":"{reason}"}}"#));
        }

        async fn next_out(&mut self) -> Outgoing {
            timeout(Duration::from_secs(1), self.from_app.recv())
                .await
                .expect("timed out waiting for outgoing")
                .expect("connection closed")
        }
    }

    async fn recv_envelope(slack: &mut SlackSocketMode) -> Value {
        timeout(Duration::from_secs(1), slack.recv())
            .await
            .expect("timed out waiting for envelope")
            .expect("driver ended")
    }

    async fn wait_state(slack: &SlackSocketMode, want: ConnectionState) {
        let start = tokio::time::Instant::now();
        while slack.state() != want {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for {want:?}, have {:?}", slack.state());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn connect_hello(dials: &mut TestDials) -> TestConn {
        let conn = dials.accept().await;
        conn.send_hello();
        conn
    }

    #[tokio::test]
    async fn event_envelope_is_delivered_and_acked() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let mut conn = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;

        conn.send_event("env-1", "inspect");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("env-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        match conn.next_out().await {
            Outgoing::Text(ack) => {
                assert_eq!(r#"{"envelope_id":"env-1"}"#, ack);
            }
            other => panic!("expected ack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn websocket_ping_is_answered_and_not_delivered() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let mut conn = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;

        conn.to_app.send(Ok(Frame::Ping(b"tick".to_vec()))).unwrap();
        match conn.next_out().await {
            Outgoing::Pong(payload) => assert_eq!(b"tick".to_vec(), payload),
            other => panic!("expected pong, got {other:?}"),
        }
        assert!(
            timeout(Duration::from_millis(30), slack.recv())
                .await
                .is_err(),
            "ping must not surface as an envelope"
        );
    }

    #[tokio::test]
    async fn hello_is_not_delivered_as_an_envelope() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let _ = conn;
        assert!(
            timeout(Duration::from_millis(30), slack.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn warning_keeps_the_old_socket_until_the_new_one_is_healthy() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let epoch = slack.epoch();

        conn1.send_disconnect("warning");
        conn1.send_event("old-1", "still-live");
        let old = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("old-1"),
            old.get("envelope_id").and_then(Value::as_str)
        );

        let conn2 = dials.accept().await;
        conn2.send_hello();
        conn2.send_event("new-1", "after-overlap");
        let new = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("new-1"),
            new.get("envelope_id").and_then(Value::as_str)
        );
        assert_eq!(epoch, slack.epoch());
        assert_eq!(ConnectionState::Up, slack.state());
    }

    #[tokio::test]
    async fn refresh_requested_overlaps_like_a_warning() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;

        conn1.send_disconnect("refresh_requested");
        let conn2 = connect_hello(&mut dials).await;
        conn2.send_event("ref-1", "ok");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("ref-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        assert_eq!(ConnectionState::Up, slack.state());
    }

    #[tokio::test]
    async fn link_disabled_rebuilds_on_a_new_generation() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let epoch = slack.epoch();

        conn1.send_disconnect("link_disabled");
        wait_state(&slack, ConnectionState::Rebuilding).await;
        let conn2 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn2.send_event("next-1", "back");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("next-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        assert!(slack.epoch() >= epoch);
        assert_ne!(epoch, slack.epoch());
    }

    #[tokio::test]
    async fn too_many_websockets_rebuilds_without_overlapping() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;

        conn1.send_disconnect("too_many_websockets");
        wait_state(&slack, ConnectionState::Rebuilding).await;
        let conn2 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn2.send_event("next-1", "back");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("next-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
    }

    #[tokio::test]
    async fn first_dial_failure_still_connects_later() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        dials.fail("apps.connections.open failed").await;
        assert_ne!(ConnectionState::Up, slack.state());

        let conn = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn.send_event("late-1", "hello");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("late-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
    }

    #[tokio::test]
    async fn silent_socket_is_treated_as_dead() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let _ = conn1;

        wait_state(&slack, ConnectionState::Rebuilding).await;
        let conn2 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn2.send_event("wd-1", "after-watchdog");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("wd-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
    }

    #[tokio::test]
    async fn closed_stream_rebuilds_without_ending_the_driver() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn1.to_app.send(Ok(Frame::Close)).unwrap();

        wait_state(&slack, ConnectionState::Rebuilding).await;
        let conn2 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn2.send_event("c-1", "rebuilt");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("c-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
    }

    #[tokio::test]
    async fn old_socket_death_during_overlap_dial_rebuilds() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let epoch = slack.epoch();

        conn1.send_disconnect("warning");
        conn1.to_app.send(Ok(Frame::Close)).unwrap();
        wait_state(&slack, ConnectionState::Rebuilding).await;

        let conn2 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        conn2.send_event("rb-1", "after-old-died");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("rb-1"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        assert_ne!(epoch, slack.epoch());
    }

    #[tokio::test]
    async fn dual_keeps_the_new_socket_if_the_old_one_dies() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let epoch = slack.epoch();

        conn1.send_disconnect("warning");
        let conn2 = dials.accept().await;
        conn1.to_app.send(Ok(Frame::Close)).unwrap();
        conn2.send_event("dual-new", "old-gone");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("dual-new"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        assert_eq!(epoch, slack.epoch());
        assert_eq!(ConnectionState::Up, slack.state());
    }

    #[tokio::test]
    async fn dual_keeps_the_old_socket_if_the_new_one_dies() {
        let (dialer, mut dials) = test_pair();
        let mut slack = start_with(dialer, test_config());
        let conn1 = connect_hello(&mut dials).await;
        wait_state(&slack, ConnectionState::Up).await;
        let epoch = slack.epoch();

        conn1.send_disconnect("warning");
        let conn2 = dials.accept().await;
        conn2.to_app.send(Ok(Frame::Close)).unwrap();
        conn1.send_event("dual-old", "new-gone");
        let envelope = recv_envelope(&mut slack).await;
        assert_eq!(
            Some("dual-old"),
            envelope.get("envelope_id").and_then(Value::as_str)
        );
        assert_eq!(epoch, slack.epoch());
        assert_eq!(ConnectionState::Up, slack.state());
    }
}
