use std::env;
use std::fs;
use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use crate::slack_socket::SlackSocketMode;
use crate::{
    AgentSession, Bridge, ControlAction, ControlCommand, HostSettings, LocalServices,
    MAX_SLACK_PHOTO_BYTES, PanePlacement, ProjectSetting, SessionStatus, SlackFile, ThreadAction,
    ThreadMessage, accepted_slack_photos, agent_display_name, apply_host_routing_policy,
    control_action, pane_placement, prompt_with_local_photos, render_session_status_card,
    self_test_message, session_update_after_agent_turn, session_update_after_prompt_paste,
    session_update_reject, setting_path, slack_file_url_is_downloadable, slack_photo_store_path,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::time::MissedTickBehavior;

pub struct RealServices {
    bot_token: String,
    current_pane_agent: Option<CurrentPaneAgent>,
}

impl RealServices {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            current_pane_agent: None,
        }
    }
}

struct CurrentPaneAgent {
    command: String,
    cwd: String,
    pane_id: String,
    self_test: Option<CurrentPaneSelfTest>,
}

struct CurrentPaneSelfTest {
    control_channel_id: String,
    thread_ts: String,
    notify_token: String,
}

impl Drop for CurrentPaneAgent {
    fn drop(&mut self) {
        // The shell is still this pane's process; leftover metadata would keep Slack driving it.
        let _ = AgentSession::clear_registry_name(&self.pane_id);
    }
}

impl LocalServices for RealServices {
    fn available_session_name(&mut self, base: &str) -> Result<String, String> {
        available_session_name(base)
    }

    fn create_session_thread(&mut self, session: AgentSession) -> Result<AgentSession, String> {
        ensure_host_daemon()?;
        let response: Value = reqwest::blocking::Client::new()
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&serde_json::json!({
                "channel": &session.control_channel,
                "text": render_session_status_card(&session),
            }))
            .send()
            .map_err(|error| format!("Slack chat.postMessage failed: {error}"))?
            .json()
            .map_err(|error| format!("Slack chat.postMessage returned invalid JSON: {error}"))?;

        if response["ok"].as_bool() != Some(true) {
            return Err(response["error"]
                .as_str()
                .unwrap_or("unknown_error")
                .to_owned());
        }

        let ts = response["ts"]
            .as_str()
            .ok_or_else(|| "Slack chat.postMessage response did not include ts".to_owned())?;
        let mut session = session;
        session.thread_ts = Some(ts.to_owned());
        Ok(session)
    }

    fn update_session_thread(&mut self, session: &AgentSession) -> Result<(), String> {
        let root_ts = session_thread(session)?;
        let response: Value = reqwest::blocking::Client::new()
            .post("https://slack.com/api/chat.update")
            .bearer_auth(&self.bot_token)
            .json(&serde_json::json!({
                "channel": &session.control_channel,
                "ts": root_ts,
                "text": render_session_status_card(session),
            }))
            .send()
            .map_err(|error| format!("Slack chat.update failed: {error}"))?
            .json()
            .map_err(|error| format!("Slack chat.update returned invalid JSON: {error}"))?;

        if response["ok"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err(response["error"]
                .as_str()
                .unwrap_or("unknown_error")
                .to_owned())
        }
    }

    fn spawn_agent(&mut self, session: AgentSession) -> Result<AgentSession, String> {
        let session = spawn_agent_tui(session)?;
        if session.placement == Some(PanePlacement::ReuseCurrentPane) {
            let pane_id = session_pane(&session)?.to_owned();
            self.current_pane_agent = Some(CurrentPaneAgent {
                command: current_pane_shell_command(
                    &pane_id,
                    &agent_command(
                        &session,
                        session.notify_socket.as_deref(),
                        session.notify_token.as_deref(),
                    ),
                ),
                cwd: session.cwd.clone(),
                pane_id,
                self_test: None,
            });
        }
        if session.thread_ts.is_none() {
            session.write_to_registry()?;
        }
        Ok(session)
    }

    fn register_session(&mut self, session: &AgentSession) -> Result<(), String> {
        if session.self_test {
            let status_file = session
                .status_file
                .as_deref()
                .ok_or_else(|| "self-test Session has no status file".to_owned())?;
            if let Err(error) = fs::remove_file(status_file)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("failed to reset self-test status file: {error}"));
            }
        }
        let registered = session
            .write_to_registry()
            .and_then(|_| ensure_host_daemon());
        if let Some(token) = session.notify_token.as_deref() {
            let signaled = tmux_status(&["wait-for", "-S", &agent_start_channel(token)]);
            if registered.is_ok() {
                signaled?;
            }
        }
        registered?;
        if session.self_test {
            let token = session
                .notify_token
                .as_deref()
                .ok_or_else(|| "self-test Session has no notification token".to_owned())?;
            if let Some(agent) = &mut self.current_pane_agent {
                agent.self_test = Some(CurrentPaneSelfTest {
                    control_channel_id: session.control_channel.clone(),
                    thread_ts: session_thread(session)?.to_owned(),
                    notify_token: token.to_owned(),
                });
            } else {
                post_self_test_prompt(
                    &self.bot_token,
                    &session.control_channel,
                    session_thread(session)?,
                    token,
                )?;
            }
        }
        Ok(())
    }

    fn list_sessions(&mut self) -> Result<Vec<AgentSession>, String> {
        AgentSession::list_from_registry()
    }

    fn attach_session(&mut self, session_name: &str) -> Result<(), String> {
        let session = AgentSession::list_from_registry()?
            .into_iter()
            .find(|session| session.session_name == session_name)
            .ok_or_else(|| "unknown session".to_owned())?;
        if !session.available {
            return Err("session is unavailable".to_owned());
        }
        if env::var_os("TMUX").is_some() {
            tmux_status(&["select-window", "-t", session_tmux_window(&session)?])?;
            tmux_status(&["select-pane", "-t", session_pane(&session)?])
        } else {
            tmux_status(&["select-pane", "-t", session_pane(&session)?])?;
            tmux_status(&["select-window", "-t", session_tmux_window(&session)?])?;
            attach_tmux_session(session_tmux_session(&session)?)
        }
    }

    fn cleanup_sessions(&mut self) -> Result<usize, String> {
        cleanup_agent_sessions(&self.bot_token)
    }
}

fn ensure_host_daemon() -> Result<(), String> {
    let socket_path = host_notify_socket_path()?;
    let startup_lock_path = format!("{socket_path}.startup.lock");
    let startup_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(startup_lock_path)
        .map_err(|error| format!("failed to open Host daemon startup lock: {error}"))?;
    startup_lock
        .lock()
        .map_err(|error| format!("failed to lock Host daemon startup: {error}"))?;
    if StdUnixStream::connect(&socket_path).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(&socket_path);
    let executable =
        env::current_exe().map_err(|error| format!("failed to find current exe: {error}"))?;
    let ready_file = env::temp_dir()
        .join(format!("pingme-daemon-ready-{}", std::process::id()))
        .display()
        .to_string();
    let _ = fs::remove_file(&ready_file);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{socket_path}.log"))
        .map_err(|error| format!("failed to open Host daemon log: {error}"))?;
    let log_file_stderr = log_file
        .try_clone()
        .map_err(|error| format!("failed to duplicate Host daemon log handle: {error}"))?;
    let mut child = Command::new(executable)
        .args(["daemon", "--ready-file", &ready_file])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_stderr))
        .spawn()
        .map_err(|error| format!("failed to spawn daemon: {error}"))?;
    for _ in 0..20 {
        if fs::metadata(&ready_file).is_ok() {
            let _ = fs::remove_file(&ready_file);
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("failed to check daemon startup: {error}"))?
        {
            return Err(format!("daemon exited before ready with {status}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    Err("daemon did not report ready within 2 seconds".to_owned())
}

fn host_notify_socket_path() -> Result<String, String> {
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .or_else(|_| {
            env::var("HOME").and_then(|home| {
                std::path::Path::new(&home)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .ok_or(env::VarError::NotPresent)
            })
        })
        .map_err(|_| "USER, LOGNAME, and HOME are unavailable for Host identity".to_owned())?;
    let safe_user: String = user
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    Ok(env::temp_dir()
        .join(format!("pingme-{safe_user}.sock"))
        .display()
        .to_string())
}

struct DaemonConfig {
    bot_token: String,
    session: AgentSession,
}

pub fn run_notify_process(args: &[String]) -> ExitCode {
    match notify_daemon(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn notify_daemon(args: &[String]) -> Result<(), String> {
    let socket_path = env::var("PINGME_NOTIFY_SOCKET")
        .map_err(|_| "missing environment variable PINGME_NOTIFY_SOCKET".to_owned())?;
    let token = env::var("PINGME_NOTIFY_TOKEN")
        .map_err(|_| "missing environment variable PINGME_NOTIFY_TOKEN".to_owned())?;
    let agent_cli = args.get(2).map(String::as_str).unwrap_or("codex");
    let notification = if agent_cli == "grok" {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .map_err(|error| format!("failed to read Grok notification: {error}"))?;
        input
    } else {
        args.get(2)
            .ok_or_else(|| "notify payload is missing".to_owned())?
            .to_owned()
    };
    let mut stream = StdUnixStream::connect(socket_path)
        .map_err(|error| format!("failed to connect daemon notify socket: {error}"))?;
    stream
        .write_all(
            serde_json::json!({ "token": token, "agent_cli": agent_cli, "notification": notification })
                .to_string()
                .as_bytes(),
        )
        .map_err(|error| format!("failed to write daemon notification: {error}"))
}

pub fn run_daemon_process(args: &[String]) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            return ExitCode::from(2);
        }
    };

    match runtime.block_on(run_host_daemon(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

async fn run_host_daemon(args: &[String]) -> Result<(), String> {
    let setting = read_file_setting()?;
    let app_token = env::var("SLACK_APP_TOKEN")
        .map_err(|_| "missing environment variable SLACK_APP_TOKEN".to_owned())?;
    let bot_token = env::var("SLACK_BOT_TOKEN")
        .map_err(|_| "missing environment variable SLACK_BOT_TOKEN".to_owned())?;
    recover_agent_sessions(&setting, &bot_token)?;
    let http = reqwest::Client::new();
    let notify_socket_path = host_notify_socket_path()?;
    let _daemon_lock = try_lock_host_daemon(&notify_socket_path)?;
    if StdUnixStream::connect(&notify_socket_path).is_ok() {
        return Err("Host daemon is already running".to_owned());
    }
    let _ = fs::remove_file(&notify_socket_path);
    let notify_listener = UnixListener::bind(&notify_socket_path)
        .map_err(|error| format!("failed to bind notify socket: {error}"))?;
    fs::set_permissions(&notify_socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to protect notify socket: {error}"))?;

    // Notify socket is the Host's local presence. Slack Socket Mode connects
    // in the background so a downed Slack handshake cannot take the daemon
    // (or Agent CLI notify) down with it.
    let mut slack = SlackSocketMode::connect(http.clone(), app_token);
    if let Ok(ready_file) = arg_value(args, "--ready-file") {
        fs::write(&ready_file, "ready\n")
            .map_err(|error| format!("failed to write daemon ready file: {error}"))?;
    }
    let mut known_sessions = tokio::task::block_in_place(AgentSession::list_from_registry)?;
    let mut pane_poll = tokio::time::interval(std::time::Duration::from_millis(1_000));
    pane_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            incoming = slack.recv() => {
                let Some(envelope) = incoming else {
                    eprintln!(
                        "Slack Socket Mode driver ended ({:?})",
                        slack.state()
                    );
                    return Ok(());
                };
                let _ = handle_control_command(&http, &bot_token, &setting, &envelope).await;
                if let Err(error) = handle_slack_event(
                    &http,
                    &bot_token,
                    &setting,
                    slack.epoch(),
                    &envelope,
                ).await {
                    let _ = post_envelope_error(
                        &http,
                        &bot_token,
                        &setting,
                        &envelope,
                        &error,
                    ).await;
                    eprintln!("failed to handle Slack event: {error}");
                }
            }
            accepted = notify_listener.accept() => {
                let (stream, _) = accepted
                    .map_err(|error| format!("failed to accept Agent CLI notification: {error}"))?;
                let notification = match read_agent_notification(stream).await {
                    Ok(notification) => notification,
                    Err(error) => {
                        eprintln!("rejected Agent CLI notification: {error}");
                        continue;
                    }
                };
                let Some(notification) = notification else {
                    continue;
                };
                if let Err(error) = handle_host_notification(
                    &http,
                    &bot_token,
                    &notification,
                ).await {
                    eprintln!("failed to handle Agent CLI notification: {error}");
                }
            }
            _ = pane_poll.tick() => {
                if let Err(error) = monitor_agent_sessions(
                    &bot_token,
                    &setting,
                    &mut known_sessions,
                ).await {
                    eprintln!("failed to monitor Agent Sessions: {error}");
                }
            }
        }
    }
}

// ponytail: O(n^2) over local Sessions; index by pane ID if a Host reaches hundreds.
fn ended_agent_sessions(known: &[AgentSession], current: &[AgentSession]) -> Vec<AgentSession> {
    let mut ended: Vec<AgentSession> = known
        .iter()
        .filter(|known| {
            known.available
                && known.status != SessionStatus::Unavailable
                && current
                    .iter()
                    .find(|current| current.pane_id == known.pane_id)
                    .is_none_or(|current| !current.available)
        })
        .cloned()
        .collect();
    ended.extend(
        current
            .iter()
            .filter(|current| {
                !current.available
                    && current.status != SessionStatus::Unavailable
                    && !known.iter().any(|known| {
                        known.pane_id == current.pane_id
                            && (known.available || known.status == SessionStatus::Unavailable)
                    })
            })
            .cloned(),
    );
    ended
}

fn pane_is_registered(sessions: &[AgentSession], pane_id: &str) -> bool {
    sessions
        .iter()
        .any(|session| session.pane_id.as_deref() == Some(pane_id))
}

async fn monitor_agent_sessions(
    bot_token: &str,
    setting: &HostSettings,
    known: &mut Vec<AgentSession>,
) -> Result<(), String> {
    let mut current = tokio::task::block_in_place(AgentSession::list_from_registry)?;
    let ended = ended_agent_sessions(known, &current);
    for ended_session in &ended {
        let mut session = ended_session.clone();
        if pane_is_registered(&current, session_pane(&session)?) {
            session.write_status(SessionStatus::Unavailable)?;
        } else {
            session.mark_unavailable();
        }
        session.mark_pane_dead();
        let config = DaemonConfig::from_session(bot_token, setting, &session);
        tokio::task::block_in_place(|| {
            publish_ended_agent_session(
                &session,
                |_| {
                    post_slack_blocking(
                        &config.bot_token,
                        &config.session.control_channel,
                        session_thread(&config.session)?,
                        &format!("{} TUI ended.", agent_display_name(&session.agent_cli)),
                    )
                },
                |_| {
                    delete_slack_thread_root(
                        &config.bot_token,
                        &config.session.control_channel,
                        session_thread(&config.session)?,
                    )
                },
            )
        })?;
        record_unavailable(known, &session);
    }
    for session in &mut current {
        if !session.available {
            session.set_status(SessionStatus::Unavailable);
        }
    }
    for session in &ended {
        record_unavailable(&mut current, session);
    }
    *known = current;
    Ok(())
}

fn record_unavailable(sessions: &mut Vec<AgentSession>, ended: &AgentSession) {
    if let Some(session) = sessions
        .iter_mut()
        .find(|session| session.pane_id == ended.pane_id)
    {
        session.mark_unavailable();
    } else {
        let mut ended = ended.clone();
        ended.mark_unavailable();
        sessions.push(ended);
    }
}

async fn read_agent_notification(stream: tokio::net::UnixStream) -> Result<Option<Value>, String> {
    const MAX_NOTIFICATION_BYTES: usize = 1_048_576;
    let mut input = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream
            .take((MAX_NOTIFICATION_BYTES + 1) as u64)
            .read_to_end(&mut input),
    )
    .await
    .map_err(|_| "notification timed out".to_owned())?
    .map_err(|error| format!("failed to read notification: {error}"))?;
    if read > MAX_NOTIFICATION_BYTES {
        return Err("notification exceeded 1 MiB".to_owned());
    }
    if input.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&input)
        .map(Some)
        .map_err(|error| format!("notification was invalid JSON: {error}"))
}

fn try_lock_host_daemon(socket_path: &str) -> Result<File, String> {
    let daemon_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{socket_path}.lock"))
        .map_err(|error| format!("failed to open Host daemon lock: {error}"))?;
    daemon_lock
        .try_lock()
        .map_err(|_| "Host daemon is already running".to_owned())?;
    Ok(daemon_lock)
}

fn read_file_setting() -> Result<HostSettings, String> {
    HostSettings::parse(
        &fs::read_to_string(setting_path())
            .map_err(|error| format!("failed to read setting.toml: {error}"))?,
    )
}

async fn handle_control_command(
    http: &reqwest::Client,
    bot_token: &str,
    setting: &HostSettings,
    envelope: &Value,
) -> Result<(), String> {
    let Some(payload) = envelope.get("payload") else {
        return Ok(());
    };
    let action = control_action(
        &ControlCommand {
            command: payload.get("command").and_then(Value::as_str).unwrap_or(""),
            channel_id: payload
                .get("channel_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            user_id: payload.get("user_id").and_then(Value::as_str).unwrap_or(""),
            text: payload.get("text").and_then(Value::as_str).unwrap_or(""),
        },
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
    );

    let ControlAction::NewSession {
        agent_cli: agent,
        project_name,
        agent_args,
    } = action
    else {
        return match action {
            ControlAction::Reject(message) => {
                post_control_response(http, bot_token, payload, &message).await
            }
            ControlAction::Ignore => Ok(()),
            ControlAction::NewSession { .. } => unreachable!(),
        };
    };

    if agent != "codex" && agent != "grok" {
        return post_control_response(
            http,
            bot_token,
            payload,
            &format!("Unknown Agent CLI: {agent}"),
        )
        .await;
    }
    let Some(project) = setting
        .projects
        .iter()
        .find(|project| project.name == project_name)
    else {
        return post_control_response(
            http,
            bot_token,
            payload,
            &format!("Unknown Project: {project_name}"),
        )
        .await;
    };

    match launch_remote_agent(http, bot_token, setting, project, &agent, agent_args).await {
        Ok(launch) => {
            let warning = launch
                .warning
                .map(|warning| format!("\nWarning: {}", warning.trim()))
                .unwrap_or_default();
            post_control_response(
                http,
                bot_token,
                payload,
                &format!("Created `{}`.{warning}", launch.session_name),
            )
            .await
        }
        Err(error) => {
            post_control_response(http, bot_token, payload, &format!("Launch failed: {error}"))
                .await
        }
    }
}

struct RemoteLaunch {
    session_name: String,
    warning: Option<String>,
}

async fn launch_remote_agent(
    http: &reqwest::Client,
    bot_token: &str,
    setting: &HostSettings,
    project: &ProjectSetting,
    agent_cli: &str,
    agent_args: Vec<String>,
) -> Result<RemoteLaunch, String> {
    let mut services = RealServices::new(bot_token.to_owned());
    let session_name = tokio::task::block_in_place(|| {
        services.available_session_name(&format!("{agent_cli}-{}", project.name))
    })?;
    let mut session = AgentSession::for_launch(
        session_name,
        agent_cli,
        &setting.host.name,
        &project.name,
        &project.cwd,
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
        pane_placement(false, current_tmux_pane().is_some()),
    );
    session.agent_args = agent_args;
    let session = tokio::task::block_in_place(|| services.create_session_thread(session))?;
    let failed_session = session.clone();
    let bridge = Bridge::new(
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
    );
    let started =
        match tokio::task::block_in_place(|| bridge.start_agent_session(session, &mut services)) {
            Ok(started) => started,
            Err(error) => {
                let _ = update_remote_launch_status(
                    http,
                    bot_token,
                    setting,
                    failed_session,
                    SessionStatus::Unavailable,
                )
                .await;
                return Err(error);
            }
        };
    if let Some(error) = started.registration_error {
        let _ = update_remote_launch_status(
            http,
            bot_token,
            setting,
            started.session,
            SessionStatus::Unavailable,
        )
        .await;
        return Err(error);
    }
    let warning = if started.status_warning.is_some() {
        update_remote_launch_status(
            http,
            bot_token,
            setting,
            started.session.clone(),
            SessionStatus::Idle,
        )
        .await
        .err()
    } else {
        None
    };
    Ok(RemoteLaunch {
        session_name: started.session.session_name.clone(),
        warning,
    })
}

async fn update_remote_launch_status(
    http: &reqwest::Client,
    bot_token: &str,
    setting: &HostSettings,
    mut session: AgentSession,
    status: SessionStatus,
) -> Result<(), String> {
    session.set_status(status);
    update_slack_root(
        http,
        &DaemonConfig::from_session(bot_token, setting, &session),
    )
    .await
}

async fn post_control_response(
    http: &reqwest::Client,
    bot_token: &str,
    payload: &Value,
    text: &str,
) -> Result<(), String> {
    if let Some(response_url) = payload.get("response_url").and_then(Value::as_str) {
        let response: Value = http
            .post(response_url)
            .json(&serde_json::json!({ "response_type": "ephemeral", "text": text }))
            .send()
            .await
            .map_err(|error| format!("Slack response_url post failed: {error}"))?
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({ "ok": true }));
        return slack_ok(&response).or(Ok(()));
    }

    let channel_id = payload
        .get("channel_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Slack command payload omitted channel_id".to_owned())?;
    let response: Value = http
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(bot_token)
        .json(&serde_json::json!({ "channel": channel_id, "text": text }))
        .send()
        .await
        .map_err(|error| format!("Slack chat.postMessage failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("Slack chat.postMessage returned invalid JSON: {error}"))?;
    slack_ok(&response)
}

impl DaemonConfig {
    fn from_session(bot_token: &str, setting: &HostSettings, session: &AgentSession) -> Self {
        let mut session = session.clone();
        session.fill_missing_from_settings(setting);
        Self {
            bot_token: bot_token.to_owned(),
            session,
        }
    }
}

fn arg_value(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("missing daemon argument {name}"))
}

async fn handle_slack_event(
    http: &reqwest::Client,
    bot_token: &str,
    setting: &HostSettings,
    started_at: f64,
    envelope: &Value,
) -> Result<(), String> {
    let Some(message) = thread_message_from_envelope(envelope) else {
        return Ok(());
    };

    let sessions = AgentSession::list_from_registry()?;
    let bridge = Bridge::new(
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
    );
    let Some(routed) = bridge.route_thread_message(&message, &sessions) else {
        return Ok(());
    };
    let action = apply_host_routing_policy(&message, routed.session, routed.action, started_at);
    let mut config = DaemonConfig::from_session(bot_token, setting, routed.session);

    match action {
        ThreadAction::Prompt(prompt) => {
            if !pane_available(session_pane(&config.session)?)? {
                mark_unavailable(http, &mut config).await?;
                return Ok(());
            }
            let prompt =
                match materialize_thread_prompt(http, &config, prompt, &message.files).await {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        post_slack(http, &config, &format!("Not executed: {error}")).await?;
                        return Ok(());
                    }
                };
            config
                .session
                .write_last_slack_prompt(Some(prompt_fingerprint(&prompt)))?;
            if let Err(error) = config.session.write_status(SessionStatus::Busy) {
                let _ = config.session.write_last_slack_prompt(None);
                post_slack(
                    http,
                    &config,
                    &format!("Not executed: failed to mark session busy: {error}"),
                )
                .await?;
                return Ok(());
            }
            if let Err(error) = update_slack_root(http, &config).await {
                let _ = config.session.write_last_slack_prompt(None);
                config.session.write_status(SessionStatus::Idle)?;
                post_slack(
                    http,
                    &config,
                    &format!("Not executed: failed to mark session busy: {error}"),
                )
                .await?;
                return Ok(());
            }
            if let Err(error) = paste_into_pane(session_pane(&config.session)?, &prompt) {
                let _ = config.session.write_last_slack_prompt(None);
                config.session.write_status(SessionStatus::Idle)?;
                let _ = update_slack_root(http, &config).await;
                post_slack(
                    http,
                    &config,
                    &session_update_after_prompt_paste(Err(error), &config.session.agent_cli),
                )
                .await?;
            } else if let Err(error) = post_slack(
                http,
                &config,
                &session_update_after_prompt_paste(Ok(()), &config.session.agent_cli),
            )
            .await
            {
                eprintln!("failed to post prompt receipt: {error}");
            }
        }
        ThreadAction::Stop => {
            if !pane_available(session_pane(&config.session)?)? {
                mark_unavailable(http, &mut config).await?;
                return Ok(());
            }
            tmux_status(&["send-keys", "-t", session_pane(&config.session)?, "Escape"])?;
            config.session.write_status(SessionStatus::Idle)?;
            update_slack_root(http, &config).await?;
            post_slack(http, &config, "Stop sent (Escape).").await?;
        }
        ThreadAction::MarkUnavailable => {
            mark_unavailable(http, &mut config).await?;
        }
        ThreadAction::Name => {
            post_slack(
                http,
                &config,
                &format!("Session Name: {}", config.session.session_name),
            )
            .await?;
        }
        ThreadAction::Rename(name) => {
            if AgentSession::name_exists(&name)? {
                post_slack(
                    http,
                    &config,
                    &format!("Not renamed: `{name}` already exists."),
                )
                .await?;
            } else {
                if !pane_available(session_pane(&config.session)?)? {
                    mark_unavailable(http, &mut config).await?;
                    return Ok(());
                }
                rename_session(http, &mut config, &name).await?;
                post_slack(http, &config, &format!("Renamed to `{name}`.")).await?;
            }
        }
        ThreadAction::Reject(reason) => {
            post_slack(http, &config, &reason).await?;
        }
        ThreadAction::Ignore => {}
    }
    Ok(())
}

async fn handle_agent_notification(
    http: &reqwest::Client,
    config: &mut DaemonConfig,
    notification: &Value,
) -> Result<(), String> {
    if notification.get("token").and_then(Value::as_str) != config.session.notify_token.as_deref() {
        return Ok(());
    }
    let Some(raw_notification) = notification.get("notification").and_then(Value::as_str) else {
        return Ok(());
    };
    let notification: Value = serde_json::from_str(raw_notification).map_err(|error| {
        format!(
            "invalid {} notification payload JSON: {error}",
            config.session.agent_cli
        )
    })?;
    let terminal_prompts = if config.session.agent_cli == "codex" {
        terminal_prompts(&notification, config.session.last_slack_prompt.as_deref())
    } else {
        Vec::new()
    };
    let response = if config.session.agent_cli == "grok" {
        let Some(response) = grok_final_response(&notification) else {
            return Ok(());
        };
        response
    } else {
        if notification.get("type").and_then(Value::as_str) != Some("agent-turn-complete") {
            return Ok(());
        }
        notification
            .get("last-assistant-message")
            .and_then(Value::as_str)
            .unwrap_or("(no final response)")
    };

    config.session.write_last_slack_prompt(None)?;
    config.session.write_status(SessionStatus::Idle)?;
    update_slack_root(http, config).await?;
    for prompt in terminal_prompts {
        post_slack(
            http,
            config,
            &format!("*You (terminal)*\n{}", slack_text(prompt)),
        )
        .await?;
    }
    post_slack(
        http,
        config,
        &session_update_after_agent_turn(&config.session.agent_cli, &slack_text(response)),
    )
    .await?;
    if write_self_test_status(
        config.session.self_test,
        config
            .session
            .status_file
            .as_deref()
            .map(std::path::Path::new),
        response,
    )? {
        config.session.write_self_test(false)?;
    }
    Ok(())
}

fn terminal_prompts<'a>(
    notification: &'a Value,
    last_slack_prompt_fingerprint: Option<&str>,
) -> Vec<&'a str> {
    let Some(messages) = notification.get("input-messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut prompts = Vec::new();
    for message in messages {
        let Some(prompt) = message
            .as_str()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        else {
            continue;
        };
        if Some(prompt_fingerprint(prompt).as_str()) != last_slack_prompt_fingerprint {
            prompts.push(prompt);
        }
    }
    prompts
}

fn prompt_fingerprint(prompt: &str) -> String {
    let mut hasher = DefaultHasher::new();
    prompt.trim().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_self_test_status(
    self_test: bool,
    status_file: Option<&std::path::Path>,
    response: &str,
) -> Result<bool, String> {
    if !self_test || response != "roundtrip-ok" {
        return Ok(false);
    }
    let status_file =
        status_file.ok_or_else(|| "self-test Session has no status file".to_owned())?;
    fs::write(status_file, "passed\n")
        .map_err(|error| format!("failed to write self-test status file: {error}"))?;
    Ok(true)
}

async fn handle_host_notification(
    http: &reqwest::Client,
    bot_token: &str,
    notification: &Value,
) -> Result<(), String> {
    let Some(token) = notification.get("token").and_then(Value::as_str) else {
        return Ok(());
    };
    let setting = read_file_setting()?;
    let sessions = AgentSession::list_from_registry()?;
    let bridge = Bridge::new(
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
    );
    let Some(session) = bridge.route_notification(token, &sessions) else {
        return Ok(());
    };
    let mut config = DaemonConfig::from_session(bot_token, &setting, session);
    if let Err(error) = handle_agent_notification(http, &mut config, notification).await {
        let _ = post_slack(http, &config, &format!("Bridge error: {error}")).await;
        return Err(error);
    }
    Ok(())
}

async fn post_envelope_error(
    http: &reqwest::Client,
    bot_token: &str,
    setting: &HostSettings,
    envelope: &Value,
    error: &str,
) -> Result<(), String> {
    let Some(message) = thread_message_from_envelope(envelope) else {
        return Ok(());
    };
    let sessions = AgentSession::list_from_registry()?;
    let bridge = Bridge::new(
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
    );
    let Some(routed) = bridge.route_thread_message(&message, &sessions) else {
        return Ok(());
    };
    let config = DaemonConfig::from_session(bot_token, setting, routed.session);
    post_slack(http, &config, &format!("Bridge error: {error}")).await
}

fn thread_message_from_envelope(envelope: &Value) -> Option<ThreadMessage<'_>> {
    let event = envelope.get("payload")?.get("event")?;
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    Some(ThreadMessage {
        channel_id: event.get("channel").and_then(Value::as_str).unwrap_or(""),
        thread_ts: event.get("thread_ts").and_then(Value::as_str),
        user_id: event.get("user").and_then(Value::as_str),
        text: event.get("text").and_then(Value::as_str).unwrap_or(""),
        has_subtype: event.get("subtype").is_some(),
        is_bot: event.get("bot_id").is_some(),
        event_ts: event
            .get("event_ts")
            .or_else(|| event.get("ts"))
            .and_then(Value::as_str),
        files: slack_files_from_event(event),
    })
}

fn slack_files_from_event(event: &Value) -> Vec<SlackFile> {
    let Some(files) = event.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    files.iter().filter_map(slack_file_from_value).collect()
}

fn slack_file_from_value(value: &Value) -> Option<SlackFile> {
    let id = value.get("id").and_then(Value::as_str)?.to_owned();
    let download_url = value
        .get("url_private_download")
        .or_else(|| value.get("url_private"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Some(SlackFile {
        id,
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        mimetype: value
            .get("mimetype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        download_url,
        size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
    })
}

async fn materialize_thread_prompt(
    http: &reqwest::Client,
    config: &DaemonConfig,
    prompt: String,
    files: &[SlackFile],
) -> Result<String, String> {
    if files.is_empty() {
        return Ok(prompt);
    }
    let photos = accepted_slack_photos(files)?;
    let mut paths = Vec::new();
    for photo in photos {
        let dest = slack_photo_store_path(session_thread(&config.session)?, photo)?;
        download_slack_photo(http, &config.bot_token, Path::new(&dest), photo).await?;
        paths.push(dest);
    }
    Ok(prompt_with_local_photos(&prompt, &paths))
}

async fn download_slack_photo(
    http: &reqwest::Client,
    bot_token: &str,
    dest: &Path,
    photo: &SlackFile,
) -> Result<(), String> {
    if !slack_file_url_is_downloadable(&photo.download_url) {
        return Err("photo URL is not a Slack file URL".to_owned());
    }
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create photo directory: {error}"))?;
    }
    let response = http
        .get(&photo.download_url)
        .bearer_auth(bot_token)
        .send()
        .await
        .map_err(|error| format!("failed to download photo: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Slack photo download returned {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read photo: {error}"))?;
    if bytes.len() as u64 > MAX_SLACK_PHOTO_BYTES {
        return Err("photo exceeds 20 MiB".to_owned());
    }
    fs::write(dest, bytes).map_err(|error| format!("failed to store photo: {error}"))
}

async fn mark_unavailable(http: &reqwest::Client, config: &mut DaemonConfig) -> Result<(), String> {
    config.session.write_status(SessionStatus::Unavailable)?;
    update_slack_root(http, config).await?;
    post_slack(
        http,
        config,
        &session_update_reject(&config.session.agent_cli, "unavailable"),
    )
    .await
}

async fn post_slack(
    http: &reqwest::Client,
    config: &DaemonConfig,
    text: &str,
) -> Result<(), String> {
    let thread_ts = session_thread(&config.session)?;
    let response: Value = http
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(&config.bot_token)
        .json(&serde_json::json!({
            "channel": &config.session.control_channel,
            "thread_ts": thread_ts,
            "text": text,
        }))
        .send()
        .await
        .map_err(|error| format!("Slack chat.postMessage failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("Slack chat.postMessage returned invalid JSON: {error}"))?;
    slack_ok(&response)
}

fn post_slack_blocking(
    bot_token: &str,
    channel_id: &str,
    thread_ts: &str,
    text: &str,
) -> Result<(), String> {
    let response: Value = reqwest::blocking::Client::new()
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(bot_token)
        .json(&serde_json::json!({
            "channel": channel_id,
            "thread_ts": thread_ts,
            "text": text,
        }))
        .send()
        .map_err(|error| format!("Slack chat.postMessage failed: {error}"))?
        .json()
        .map_err(|error| format!("Slack chat.postMessage returned invalid JSON: {error}"))?;
    slack_ok(&response)
}

fn slack_ok(response: &Value) -> Result<(), String> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "Slack API error: {}",
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
        ))
    }
}

fn slack_delete_ok(response: &Value) -> Result<(), String> {
    if response.get("error").and_then(Value::as_str) == Some("message_not_found") {
        Ok(())
    } else {
        slack_ok(response)
    }
}

fn delete_slack_thread_root(
    bot_token: &str,
    channel_id: &str,
    root_ts: &str,
) -> Result<(), String> {
    let response: Value = reqwest::blocking::Client::new()
        .post("https://slack.com/api/chat.delete")
        .bearer_auth(bot_token)
        .json(&serde_json::json!({
            "channel": channel_id,
            "ts": root_ts,
        }))
        .send()
        .map_err(|error| format!("Slack chat.delete failed: {error}"))?
        .json()
        .map_err(|error| format!("Slack chat.delete returned invalid JSON: {error}"))?;
    slack_delete_ok(&response)
}

fn available_session_name(base: &str) -> Result<String, String> {
    if !AgentSession::name_exists(base)? {
        return Ok(base.to_owned());
    }
    for suffix in 2..100 {
        let candidate = format!("{base}-{suffix}");
        if !AgentSession::name_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(format!("no available session name for {base}"))
}

fn publish_ended_agent_session(
    session: &AgentSession,
    mut post_tui_ended: impl FnMut(&AgentSession) -> Result<(), String>,
    mut delete_thread_root: impl FnMut(&AgentSession) -> Result<(), String>,
) -> Result<(), String> {
    post_tui_ended(session)?;
    if session.thread_ts.is_none() {
        return Ok(());
    }
    delete_thread_root(session)
}

fn cleanup_unavailable_sessions(
    sessions: impl IntoIterator<Item = AgentSession>,
    mut delete_thread_root: impl FnMut(&AgentSession) -> Result<(), String>,
    mut remove_local_mapping: impl FnMut(&AgentSession) -> Result<(), String>,
) -> Result<usize, String> {
    let mut cleaned = 0;
    for session in sessions.into_iter().filter(|session| !session.available) {
        if session.thread_ts.is_some() {
            delete_thread_root(&session)?;
        }
        remove_local_mapping(&session)?;
        cleaned += 1;
    }
    Ok(cleaned)
}

fn cleanup_agent_sessions(bot_token: &str) -> Result<usize, String> {
    let setting = read_file_setting()?;
    cleanup_unavailable_sessions(
        AgentSession::list_from_registry()?,
        |session| {
            let mut session = session.clone();
            session.fill_missing_from_settings(&setting);
            delete_slack_thread_root(
                bot_token,
                &session.control_channel,
                session_thread(&session)?,
            )
        },
        |session| tmux_status(&["kill-window", "-t", session_tmux_window(session)?]),
    )
}

fn recover_agent_sessions(setting: &HostSettings, bot_token: &str) -> Result<(), String> {
    let mut services = RealServices::new(bot_token.to_owned());
    for session in AgentSession::list_from_registry()? {
        if !session.available {
            mark_session_unavailable(&mut services, setting, &session)?;
        }
    }
    Ok(())
}

fn mark_session_unavailable(
    services: &mut RealServices,
    setting: &HostSettings,
    session: &AgentSession,
) -> Result<(), String> {
    if session.thread_ts.is_none() {
        return Ok(());
    }
    let mut session = session.clone();
    session.fill_missing_from_settings(setting);
    session.set_status(SessionStatus::Unavailable);
    services.update_session_thread(&session)
}

fn session_pane(session: &AgentSession) -> Result<&str, String> {
    session
        .pane_id
        .as_deref()
        .ok_or_else(|| format!("Agent Session {} has no tmux pane", session.session_name))
}

fn session_thread(session: &AgentSession) -> Result<&str, String> {
    session.thread_ts.as_deref().ok_or_else(|| {
        format!(
            "Agent Session {} has no Session Thread",
            session.session_name
        )
    })
}

fn session_tmux_window(session: &AgentSession) -> Result<&str, String> {
    session
        .tmux_window
        .as_deref()
        .ok_or_else(|| format!("Agent Session {} has no tmux window", session.session_name))
}

fn session_tmux_session(session: &AgentSession) -> Result<&str, String> {
    session
        .tmux_session
        .as_deref()
        .ok_or_else(|| format!("Agent Session {} has no tmux session", session.session_name))
}

fn pane_available(pane_id: &str) -> Result<bool, String> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{pane_id}\t#{pane_dead}",
        ])
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("failed to check tmux pane: {error}"))?;
    if !output.status.success() {
        return Ok(false);
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let mut parts = output.trim().split('\t');
    Ok(parts.next() == Some(pane_id) && parts.next() == Some("0"))
}

async fn rename_session(
    http: &reqwest::Client,
    config: &mut DaemonConfig,
    name: &str,
) -> Result<(), String> {
    tmux_status(&["rename-window", "-t", session_pane(&config.session)?, name])?;
    config.session.write_name(name)?;
    paste_into_pane(session_pane(&config.session)?, &format!("/rename {name}"))?;
    update_slack_root(http, config).await
}

async fn update_slack_root(http: &reqwest::Client, config: &DaemonConfig) -> Result<(), String> {
    let thread_ts = session_thread(&config.session)?;
    let response: Value = http
        .post("https://slack.com/api/chat.update")
        .bearer_auth(&config.bot_token)
        .json(&serde_json::json!({
            "channel": &config.session.control_channel,
            "ts": thread_ts,
            "text": render_session_status_card(&config.session),
        }))
        .send()
        .await
        .map_err(|error| format!("Slack chat.update failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("Slack chat.update returned invalid JSON: {error}"))?;
    slack_ok(&response)
}

fn paste_into_pane(pane_id: &str, text: &str) -> Result<(), String> {
    let buffer = format!("pingme-{}", std::process::id());
    let mut load = Command::new("tmux")
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to load tmux buffer: {error}"))?;
    load.stdin
        .take()
        .ok_or_else(|| "failed to open tmux buffer stdin".to_owned())?
        .write_all(text.as_bytes())
        .map_err(|error| format!("failed to write tmux buffer: {error}"))?;
    let status = load
        .wait()
        .map_err(|error| format!("failed waiting for tmux load-buffer: {error}"))?;
    if !status.success() {
        return Err(format!("tmux load-buffer exited with {status}"));
    }
    tmux_status(&["paste-buffer", "-d", "-b", &buffer, "-t", pane_id])?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    tmux_status(&["send-keys", "-t", pane_id, "Enter"])
}

fn spawn_agent_tui(session: AgentSession) -> Result<AgentSession, String> {
    // Leave the pane unpublished until write_to_registry; Session Name is the last field.
    if session.agent_cli == "grok" {
        install_grok_hook()?;
    }
    let notify_socket = session
        .thread_ts
        .as_ref()
        .map(|_| host_notify_socket_path())
        .transpose()?;
    let notify_token = session.thread_ts.as_deref().map(|thread_ts| {
        format!(
            "{}-{}-{thread_ts}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        )
    });

    match session
        .placement
        .ok_or_else(|| "launch Session has no pane placement".to_owned())?
    {
        PanePlacement::ReuseCurrentPane => {
            let pane_id = current_tmux_pane()
                .ok_or_else(|| "current terminal is not the configured TMUX_PANE".to_owned())?;
            let mut session = session;
            session.pane_id = Some(pane_id);
            session.notify_socket = notify_socket;
            session.notify_token = notify_token;
            Ok(session)
        }
        PanePlacement::DetachedWindow => {
            let pane_id = tmux_output(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}\t#{window_id}",
                "-n",
                &session.session_name,
                "-c",
                &session.cwd,
                &agent_command(&session, notify_socket.as_deref(), notify_token.as_deref()),
            ])?;
            let (pane_id, window_id) = parse_tmux_window(&pane_id)?;
            let pane_id = pane_id.to_owned();
            tmux_status(&["set-option", "-w", "-t", &pane_id, "remain-on-exit", "on"])?;
            let mut session = session;
            session.pane_id = Some(pane_id);
            session.tmux_window = Some(window_id.to_owned());
            session.notify_socket = notify_socket;
            session.notify_token = notify_token;
            Ok(session)
        }
        PanePlacement::DetachedSession { .. } => {
            let pane_id = tmux_output(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-s",
                &session.session_name,
                "-n",
                &session.agent_cli,
                "-c",
                &session.cwd,
                &agent_command(&session, notify_socket.as_deref(), notify_token.as_deref()),
            ])?;
            let pane_id = pane_id.trim().to_owned();
            tmux_status(&["set-option", "-w", "-t", &pane_id, "remain-on-exit", "on"])?;
            let tmux_session = session.session_name.clone();
            let mut session = session;
            session.pane_id = Some(pane_id);
            session.tmux_session = Some(tmux_session);
            session.notify_socket = notify_socket;
            session.notify_token = notify_token;
            Ok(session)
        }
    }
}

fn parse_tmux_window(output: &str) -> Result<(&str, &str), String> {
    output
        .trim()
        .split_once('\t')
        .ok_or_else(|| "tmux new-window did not return pane and window IDs".to_owned())
}

pub fn attach_or_switch_agent_tui(
    session: &AgentSession,
    services: &mut RealServices,
) -> Result<(), String> {
    match session.placement {
        Some(PanePlacement::DetachedSession { attach: true }) => attach_tmux_session(
            session
                .tmux_session
                .as_deref()
                .unwrap_or(&session.session_name),
        ),
        Some(PanePlacement::ReuseCurrentPane) => {
            let agent = services
                .current_pane_agent
                .take()
                .ok_or_else(|| "missing Agent CLI command for the current pane".to_owned())?;
            run_agent_in_current_pane(&agent, &services.bot_token)
        }
        Some(PanePlacement::DetachedWindow | PanePlacement::DetachedSession { attach: false })
        | None => Ok(()),
    }
}

fn run_agent_in_current_pane(agent: &CurrentPaneAgent, bot_token: &str) -> Result<(), String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&agent.command)
        .current_dir(&agent.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to start Agent CLI: {error}"))?;
    let posted = match &agent.self_test {
        Some(self_test) => post_self_test_prompt(
            bot_token,
            &self_test.control_channel_id,
            &self_test.thread_ts,
            &self_test.notify_token,
        ),
        None => Ok(()),
    };
    child
        .wait()
        .map_err(|error| format!("failed to wait for Agent CLI: {error}"))?;
    posted
}

fn current_pane_shell_command(pane_id: &str, command: &str) -> String {
    // Keep a shell so EXIT still unsets pane metadata if the Agent CLI is interrupted.
    let without_exec = command.replace("; exec ", "; ");
    let without_exec = without_exec
        .strip_prefix("exec ")
        .map(str::to_owned)
        .unwrap_or(without_exec);
    format!(
        "trap {} EXIT; {}",
        shell_quote(&AgentSession::clear_registry_name_command(pane_id)),
        without_exec
    )
}

fn post_self_test_prompt(
    bot_token: &str,
    control_channel_id: &str,
    thread_ts: &str,
    notify_token: &str,
) -> Result<(), String> {
    std::thread::sleep(std::time::Duration::from_secs(1));
    post_slack_blocking(
        bot_token,
        control_channel_id,
        thread_ts,
        &self_test_message(notify_token),
    )
}

fn attach_tmux_session(target: &str) -> Result<(), String> {
    let status = Command::new("tmux")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .args(["attach-session", "-t", target])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to attach tmux session: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tmux attach-session exited with {status}"))
    }
}

fn agent_command(
    session: &AgentSession,
    notify_socket_path: Option<&str>,
    notify_token: Option<&str>,
) -> String {
    let command = if session.agent_cli == "grok" {
        grok_command(session, notify_socket_path, notify_token)
    } else {
        codex_command(session, notify_socket_path, notify_token)
    };
    let command = append_agent_args(command, &session.agent_args);
    match notify_token {
        Some(token) => format!(
            "tmux wait-for {}; {command}",
            shell_quote(&agent_start_channel(token))
        ),
        None => command,
    }
}

fn append_agent_args(mut command: String, args: &[String]) -> String {
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

fn agent_start_channel(token: &str) -> String {
    format!("pingme-start-{token}")
}

fn codex_command(
    session: &AgentSession,
    notify_socket_path: Option<&str>,
    notify_token: Option<&str>,
) -> String {
    match (
        session.thread_ts.as_deref(),
        notify_socket_path,
        notify_token,
        notify_arg(),
    ) {
        (Some(thread_ts), Some(socket_path), Some(token), Some(notify)) => format!(
            "exec env PINGME_SESSION_NAME={} PINGME_THREAD_TS={} PINGME_NOTIFY_SOCKET={} PINGME_NOTIFY_TOKEN={} codex -c {}",
            shell_quote(&session.session_name),
            shell_quote(thread_ts),
            shell_quote(socket_path),
            shell_quote(token),
            shell_quote(&format!("notify={notify}")),
        ),
        (Some(thread_ts), _, _, _) => format!(
            "exec env PINGME_SESSION_NAME={} PINGME_THREAD_TS={} codex",
            shell_quote(&session.session_name),
            shell_quote(thread_ts),
        ),
        (None, _, _, _) => "exec codex".to_owned(),
    }
}

fn grok_command(
    session: &AgentSession,
    notify_socket_path: Option<&str>,
    notify_token: Option<&str>,
) -> String {
    match (
        session.thread_ts.as_deref(),
        notify_socket_path,
        notify_token,
    ) {
        (Some(thread_ts), Some(socket_path), Some(token)) => format!(
            "exec env PINGME_SESSION_NAME={} PINGME_THREAD_TS={} PINGME_NOTIFY_SOCKET={} PINGME_NOTIFY_TOKEN={} PINGME_EXECUTABLE={} grok",
            shell_quote(&session.session_name),
            shell_quote(thread_ts),
            shell_quote(socket_path),
            shell_quote(token),
            shell_quote(&env::current_exe().unwrap_or_default().display().to_string()),
        ),
        _ => "exec grok".to_owned(),
    }
}

const GROK_HOOK: &str = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"test -n \"${PINGME_NOTIFY_SOCKET:-}\" && test -n \"${PINGME_EXECUTABLE:-}\" && \"${PINGME_EXECUTABLE}\" notify grok || true","timeout":5}]}]}}"#;

fn install_grok_hook() -> Result<(), String> {
    let home = env::var("HOME")
        .map_err(|_| "missing environment variable HOME for Grok hook".to_owned())?;
    let hooks = std::path::Path::new(&home).join(".grok/hooks");
    fs::create_dir_all(&hooks)
        .map_err(|error| format!("failed to create Grok hook directory: {error}"))?;
    let hook = hooks.join("pingme.json");
    if hook.exists() {
        let existing = fs::read_to_string(&hook)
            .map_err(|error| format!("failed to read Grok completion hook: {error}"))?;
        if existing == GROK_HOOK {
            return Ok(());
        }
        return Err(format!(
            "Grok completion hook already exists with different content: {}",
            hook.display()
        ));
    }
    fs::write(hook, GROK_HOOK)
        .map_err(|error| format!("failed to install Grok completion hook: {error}"))
}

fn grok_final_response(notification: &Value) -> Option<&str> {
    if !matches!(
        notification.get("hookEventName").and_then(Value::as_str),
        Some("stop" | "Stop")
    ) || notification.get("reason").and_then(Value::as_str) != Some("end_turn")
        || notification
            .get("subagentType")
            .is_some_and(|value| !value.is_null())
    {
        return None;
    }
    Some(
        notification
            .get("lastAssistantMessage")
            .and_then(Value::as_str)
            .unwrap_or("(no final response)"),
    )
}

fn notify_arg() -> Option<String> {
    let executable = env::current_exe().ok()?.display().to_string();
    serde_json::to_string(&vec![executable, "notify".to_owned()]).ok()
}

pub fn current_tmux_pane() -> Option<String> {
    let pane_id = env::var("TMUX_PANE")
        .ok()
        .filter(|pane_id| !pane_id.is_empty())?;
    let current_tty = Command::new("tty")
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !current_tty.status.success() {
        return None;
    }
    let current_tty = String::from_utf8(current_tty.stdout).ok()?;
    let pane_tty = tmux_output(&["display-message", "-p", "-t", &pane_id, "#{pane_tty}"]).ok()?;
    terminal_paths_match(&current_tty, &pane_tty).then_some(pane_id)
}

fn terminal_paths_match(current_tty: &str, pane_tty: &str) -> bool {
    current_tty.trim() == pane_tty.trim()
}

fn tmux_output(args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|error| format!("failed to run tmux: {error}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("tmux output was not UTF-8: {error}"))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn tmux_status(args: &[&str]) -> Result<(), String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|error| format!("failed to run tmux: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn slack_text(text: &str) -> String {
    const LIMIT: usize = 3_500;
    if text.chars().count() <= LIMIT {
        text.to_owned()
    } else {
        format!(
            "{}\n[truncated by pingme]",
            text.chars().take(LIMIT).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn recorded_slack_envelopes() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/slack-message-envelopes.json"
        ))
        .unwrap()
    }

    fn test_session(
        name: &str,
        pane_id: &str,
        thread_ts: Option<&str>,
        control_channel: &str,
        operator: &str,
    ) -> AgentSession {
        let session = AgentSession::for_launch(
            name,
            "codex",
            "linux",
            "project",
            "/work/project",
            control_channel,
            operator,
            PanePlacement::DetachedWindow,
        );
        let mut session = session;
        if let Some(thread_ts) = thread_ts {
            session.thread_ts = Some(thread_ts.to_owned());
        }
        session.pane_id = Some(pane_id.to_owned());
        session.tmux_session = Some("bridge".to_owned());
        session.tmux_window = Some("bridge:3".to_owned());
        session.notify_socket = Some("/tmp/bridge.sock".to_owned());
        session.notify_token = Some("secret".to_owned());
        session.set_status(SessionStatus::Idle);
        session
    }

    #[test]
    fn recorded_operator_envelope_routes_as_a_prompt() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();
        let action = apply_host_routing_policy(&message, routed.session, routed.action, 0.0);

        assert_eq!(
            ThreadAction::Prompt("inspect the failure".to_owned()),
            action
        );
    }

    #[test]
    fn recorded_operator_prompt_is_rejected_while_session_is_busy() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator"]).unwrap();
        let mut session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        session.set_status(SessionStatus::Busy);
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];
        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        let action = apply_host_routing_policy(&message, routed.session, routed.action, 0.0);

        assert_eq!(
            ThreadAction::Reject(session_update_reject("codex", "busy")),
            action
        );
    }

    #[test]
    fn recorded_stale_operator_prompt_is_rejected() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];
        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        let action =
            apply_host_routing_policy(&message, routed.session, routed.action, 1757221925.0);

        assert_eq!(
            ThreadAction::Reject(session_update_reject("codex", "stale")),
            action
        );
    }

    #[test]
    fn recorded_operator_prompt_is_rejected_for_unavailable_session() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator"]).unwrap();
        let mut session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        session.mark_pane_dead();
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];
        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        let action = apply_host_routing_policy(&message, routed.session, routed.action, 0.0);

        assert_eq!(ThreadAction::MarkUnavailable, action);
    }

    #[test]
    fn recorded_pingme_stop_decides_to_stop_without_ending_the_session() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator_stop"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];
        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        let action = apply_host_routing_policy(&message, routed.session, routed.action, 0.0);

        assert_eq!(ThreadAction::Stop, action);
        assert!(routed.session.available);
        assert_eq!(SessionStatus::Idle, routed.session.status);
    }

    #[test]
    fn recorded_other_member_envelope_is_rejected() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["other_member"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(
            ThreadAction::Reject(session_update_reject("codex", "unauthorized")),
            routed.action
        );
    }

    #[test]
    fn recorded_non_operator_envelopes_never_route_as_prompts() {
        let envelopes = recorded_slack_envelopes();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        for (fixture, expected) in [
            (
                "missing_identity",
                Some(ThreadAction::Reject(session_update_reject(
                    "codex",
                    "unauthorized",
                ))),
            ),
            ("bot_message", Some(ThreadAction::Ignore)),
            ("message_changed", None),
        ] {
            let message = thread_message_from_envelope(&envelopes[fixture]).unwrap();
            let action = bridge
                .route_thread_message(&message, &sessions)
                .map(|routed| routed.action);

            assert_eq!(expected, action, "{fixture}");
        }
    }

    #[test]
    fn recorded_operator_png_file_share_routes_as_a_prompt() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator_png_file_share"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(ThreadAction::Prompt(String::new()), routed.action);
        assert_eq!("F123PNG", message.files[0].id);
        assert_eq!("image/png", message.files[0].mimetype);
    }

    #[test]
    fn recorded_operator_jpeg_caption_routes_as_a_prompt() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator_jpeg_caption"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(
            ThreadAction::Prompt("what is wrong here".to_owned()),
            routed.action
        );
        assert_eq!("image/jpeg", message.files[0].mimetype);
    }

    #[test]
    fn recorded_operator_pdf_is_rejected() {
        let envelopes = recorded_slack_envelopes();
        let message = thread_message_from_envelope(&envelopes["operator_pdf"]).unwrap();
        let session = test_session(
            "codex-project",
            "%7",
            Some("1757221923.123456"),
            "C_CONTROL",
            "U_OPERATOR",
        );
        let bridge = Bridge::new("C_CONTROL", "U_OPERATOR");
        let sessions = [session];

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(
            ThreadAction::Reject("not executed: unsupported file type: notes.pdf".to_owned()),
            routed.action
        );
    }

    #[test]
    fn tui_ended_deletes_the_session_thread_root_afterward() {
        let session = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        let events = RefCell::new(Vec::new());

        publish_ended_agent_session(
            &session,
            |session| {
                events.borrow_mut().push(format!(
                    "tui-ended:{}:{}",
                    session.control_channel,
                    session.thread_ts.as_deref().unwrap_or_default()
                ));
                Ok(())
            },
            |session| {
                events.borrow_mut().push(format!(
                    "delete-root:{}:{}",
                    session.control_channel,
                    session.thread_ts.as_deref().unwrap_or_default()
                ));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            vec!["tui-ended:C1:100.1", "delete-root:C1:100.1"],
            events.into_inner()
        );
    }

    #[test]
    fn tui_ended_without_a_thread_does_not_delete_a_root() {
        let session = test_session("codex-project", "%7", None, "C1", "U1");
        let deleted = RefCell::new(false);

        publish_ended_agent_session(
            &session,
            |_| Ok(()),
            |_| {
                *deleted.borrow_mut() = true;
                Ok(())
            },
        )
        .unwrap();

        assert!(!*deleted.borrow());
    }

    #[test]
    fn failed_tui_ended_preserves_the_session_thread_root() {
        let session = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        let deleted = RefCell::new(false);

        let result = publish_ended_agent_session(
            &session,
            |_| Err("Slack API error: ratelimited".to_owned()),
            |_| {
                *deleted.borrow_mut() = true;
                Ok(())
            },
        );

        assert_eq!(Err("Slack API error: ratelimited".to_owned()), result);
        assert!(!*deleted.borrow());
    }

    #[test]
    fn cleanup_deletes_session_thread_root_before_local_mapping() {
        let mut session = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        session.mark_unavailable();
        let events = RefCell::new(Vec::new());

        let cleaned = cleanup_unavailable_sessions(
            vec![session],
            |session| {
                events.borrow_mut().push(format!(
                    "delete-root:{}:{}",
                    session.control_channel,
                    session.thread_ts.as_deref().unwrap_or_default()
                ));
                Ok(())
            },
            |session| {
                events.borrow_mut().push(format!(
                    "remove-mapping:{}",
                    session.tmux_window.as_deref().unwrap_or_default()
                ));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(1, cleaned);
        assert_eq!(
            vec!["delete-root:C1:100.1", "remove-mapping:bridge:3"],
            events.into_inner()
        );
    }

    #[test]
    fn cleanup_accepts_an_already_missing_session_thread_root() {
        let mut session = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        session.mark_unavailable();
        let removed = RefCell::new(Vec::new());

        let cleaned = cleanup_unavailable_sessions(
            vec![session],
            |_| {
                slack_delete_ok(&serde_json::json!({
                    "ok": false,
                    "error": "message_not_found"
                }))
            },
            |session| {
                removed
                    .borrow_mut()
                    .push(session.tmux_window.clone().unwrap_or_default());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(1, cleaned);
        assert_eq!(vec!["bridge:3"], removed.into_inner());
    }

    #[test]
    fn cleanup_preserves_local_mapping_when_thread_root_deletion_fails() {
        let mut session = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        session.mark_unavailable();
        let removed = RefCell::new(Vec::new());

        let result = cleanup_unavailable_sessions(
            vec![session],
            |_| Err("Slack API error: ratelimited".to_owned()),
            |session| {
                removed
                    .borrow_mut()
                    .push(session.tmux_window.clone().unwrap_or_default());
                Ok(())
            },
        );

        assert_eq!(Err("Slack API error: ratelimited".to_owned()), result);
        assert!(removed.into_inner().is_empty());
    }

    #[test]
    fn tmux_new_window_output_keeps_pane_and_window_targets_distinct() {
        assert_eq!(("%7", "@3"), parse_tmux_window("%7\t@3\n").unwrap());
    }

    #[test]
    fn bridged_agent_waits_for_session_registration_before_starting() {
        let session = AgentSession::for_launch(
            "codex-pingme",
            "codex",
            "linux",
            "project",
            "/work/pingme",
            "C1",
            "U1",
            PanePlacement::DetachedWindow,
        );
        let mut session = session;
        session.thread_ts = Some("100.1".to_owned());
        let command = agent_command(&session, Some("/tmp/pingme.sock"), Some("secret"));

        assert!(command.starts_with("tmux wait-for 'pingme-start-secret'; exec env "));
    }

    #[test]
    fn grok_launch_command_appends_resume_args() {
        let mut session = AgentSession::for_launch(
            "grok-pingme",
            "grok",
            "linux",
            "project",
            "/work/pingme",
            "C1",
            "U1",
            PanePlacement::DetachedWindow,
        );
        session.thread_ts = Some("100.1".to_owned());
        session.agent_args = vec![
            "--resume".to_owned(),
            "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned(),
        ];
        let command = agent_command(&session, Some("/tmp/pingme.sock"), Some("secret"));

        assert!(
            command.ends_with(" grok '--resume' '01a090da-d3ba-7bb1-8cc0-aab11beccab6'"),
            "{command}"
        );
        assert!(command.starts_with("tmux wait-for 'pingme-start-secret'; exec env "));
    }

    #[test]
    fn unbridged_grok_launch_command_appends_resume_args() {
        let mut session = AgentSession::for_launch(
            "grok-unbridged",
            "grok",
            "",
            "unknown",
            "/work/pingme",
            "",
            "",
            PanePlacement::ReuseCurrentPane,
        );
        session.agent_args = vec![
            "--resume".to_owned(),
            "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned(),
        ];
        let command = agent_command(&session, None, None);

        assert_eq!(
            "exec grok '--resume' '01a090da-d3ba-7bb1-8cc0-aab11beccab6'",
            command
        );
    }

    #[test]
    fn unbridged_codex_launch_command_forwards_trailing_args() {
        let mut session = AgentSession::for_launch(
            "codex-pingme",
            "codex",
            "linux",
            "project",
            "/work/pingme",
            "C1",
            "U1",
            PanePlacement::DetachedWindow,
        );
        session.agent_args = vec!["resume".to_owned(), "session-id".to_owned()];
        let command = agent_command(&session, None, None);

        assert_eq!("exec codex 'resume' 'session-id'", command);
    }

    #[test]
    fn bridged_codex_forwards_trailing_args_after_notify() {
        let mut session = AgentSession::for_launch(
            "codex-pingme",
            "codex",
            "linux",
            "project",
            "/work/pingme",
            "C1",
            "U1",
            PanePlacement::DetachedWindow,
        );
        session.thread_ts = Some("100.1".to_owned());
        session.agent_args = vec!["resume".to_owned(), "session-id".to_owned()];
        let command = agent_command(&session, Some("/tmp/pingme.sock"), Some("secret"));

        assert!(command.contains(" codex -c "), "{command}");
        assert!(command.ends_with(" 'resume' 'session-id'"), "{command}");
        assert!(command.starts_with("tmux wait-for 'pingme-start-secret'; exec env "));
    }

    #[test]
    fn current_pane_shell_keeps_wait_for_and_unsets_metadata_on_exit() {
        let command = current_pane_shell_command(
            "%7",
            "tmux wait-for 'pingme-start-secret'; exec env PINGME_SESSION_NAME='codex-pingme' codex",
        );

        assert_eq!(
            format!(
                "trap {} EXIT; tmux wait-for 'pingme-start-secret'; env PINGME_SESSION_NAME='codex-pingme' codex",
                shell_quote(&AgentSession::clear_registry_name_command("%7"))
            ),
            command
        );
    }

    #[test]
    fn inherited_tmux_pane_must_match_the_current_terminal() {
        assert!(terminal_paths_match("/dev/pts/31\n", "/dev/pts/31\n"));
        assert!(!terminal_paths_match("/dev/pts/31\n", "/dev/pts/19\n"));
    }

    #[test]
    fn host_daemon_lock_rejects_a_second_owner() {
        let socket_path = env::temp_dir()
            .join(format!("pingme-lock-test-{}", std::process::id()))
            .display()
            .to_string();
        let first = try_lock_host_daemon(&socket_path).unwrap();

        assert!(try_lock_host_daemon(&socket_path).is_err());

        drop(first);
        let _ = fs::remove_file(format!("{socket_path}.lock"));
    }

    #[test]
    fn parses_named_tmux_agent_session_metadata_in_one_place() {
        let session = AgentSession::from_registry_record(
            "pane_id=%7\ttmux_session=bridge\ttmux_window=bridge:3\tpane_dead=0\tsession_name=codex-project\tagent_cli=codex\thost=linux\tproject=project\tcwd=/work/project\tstatus=idle\tthread_ts=100.1\tcontrol_channel=C1\toperator=U1\tnotify_socket=/tmp/bridge.sock\tnotify_token=secret\tself_test=1\tstatus_file=/tmp/status\tlast_slack_prompt=abc123",
        )
        .unwrap()
        .unwrap();

        assert_eq!(Some("bridge:3"), session.tmux_window.as_deref());
        assert_eq!("codex-project", session.session_name);
        assert_eq!(Some("secret"), session.notify_token.as_deref());
        assert!(session.self_test);
        assert_eq!(Some("/tmp/status"), session.status_file.as_deref());
        assert_eq!(Some("abc123"), session.last_slack_prompt.as_deref());
        assert!(session.available);
    }

    #[test]
    fn self_test_writes_status_only_for_roundtrip_ok() {
        let status_file =
            env::temp_dir().join(format!("pingme-self-test-status-{}", std::process::id()));
        let _ = fs::remove_file(&status_file);

        assert!(!write_self_test_status(true, Some(&status_file), "not-ok").unwrap());
        assert!(!status_file.exists());
        assert!(!write_self_test_status(true, Some(&status_file), "roundtrip-ok\n").unwrap());
        assert!(!status_file.exists());
        assert!(write_self_test_status(true, Some(&status_file), "roundtrip-ok").unwrap());
        assert_eq!("passed\n", fs::read_to_string(&status_file).unwrap());

        let _ = fs::remove_file(status_file);
    }

    #[test]
    fn codex_notification_mirrors_all_terminal_input() {
        let notification = serde_json::json!({
            "type": "agent-turn-complete",
            "input-messages": ["first terminal prompt", "second terminal prompt"],
            "last-assistant-message": "done"
        });

        assert_eq!(
            vec!["first terminal prompt", "second terminal prompt"],
            terminal_prompts(&notification, None)
        );
        assert_eq!(
            vec!["first terminal prompt"],
            terminal_prompts(
                &notification,
                Some(&prompt_fingerprint("second terminal prompt"))
            )
        );
    }

    #[test]
    fn pane_monitor_detects_an_ended_agent_session_once() {
        let live = test_session("codex-project", "%7", Some("100.1"), "C1", "U1");
        let mut unavailable = live.clone();
        unavailable.mark_pane_dead();
        let mut already_reported = live.clone();
        already_reported.mark_unavailable();

        assert_eq!(
            1,
            ended_agent_sessions(
                std::slice::from_ref(&live),
                std::slice::from_ref(&unavailable)
            )
            .len()
        );
        assert_eq!(
            1,
            ended_agent_sessions(&[], std::slice::from_ref(&unavailable)).len()
        );
        assert!(!pane_is_registered(
            &[],
            live.pane_id.as_deref().unwrap_or_default()
        ));
        assert!(pane_is_registered(
            std::slice::from_ref(&unavailable),
            live.pane_id.as_deref().unwrap_or_default()
        ));
        assert!(
            ended_agent_sessions(&[already_reported], std::slice::from_ref(&unavailable))
                .is_empty()
        );
    }

    #[test]
    fn pane_monitor_records_each_success_before_the_next_publication() {
        let first = test_session("first", "%7", Some("100.1"), "C1", "U1");
        let second = test_session("second", "%8", Some("100.2"), "C1", "U1");
        let mut known = vec![first.clone(), second];

        record_unavailable(&mut known, &first);

        assert!(!known[0].available);
        assert_eq!(SessionStatus::Unavailable, known[0].status);
        assert!(known[1].available);
    }

    #[test]
    fn accepts_only_top_level_grok_stop_completion() {
        let completion = serde_json::json!({
            "hookEventName": "stop",
            "reason": "end_turn",
            "lastAssistantMessage": "done"
        });
        assert_eq!(Some("done"), grok_final_response(&completion));

        let subagent = serde_json::json!({
            "hookEventName": "stop",
            "reason": "end_turn",
            "subagentType": "explore",
            "lastAssistantMessage": "child result"
        });
        assert_eq!(None, grok_final_response(&subagent));
    }

    #[test]
    fn uses_grok_user_hook_location() {
        let path = std::path::Path::new("/home/test").join(".grok/hooks/pingme.json");
        assert_eq!(
            "/home/test/.grok/hooks/pingme.json",
            path.display().to_string()
        );
        assert!(GROK_HOOK.contains("PINGME_NOTIFY_SOCKET"));
    }
}
