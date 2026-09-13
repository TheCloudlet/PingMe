use std::collections::BTreeMap;

mod bridge;
mod host;
mod session;
mod slack_socket;

pub use bridge::{Bridge, RoutedThreadAction, StartedSession};
pub use host::{RealServices, attach_or_switch_agent_tui, run_daemon_process, run_notify_process};
pub use session::{AgentSession, HostSettings, PanePlacement, ProjectSetting, SessionStatus};

pub const SELF_TEST_PROMPT: &str = "Respond with exactly roundtrip-ok";

pub fn self_test_message(token: &str) -> String {
    format!("__pingme_self_test__:{token}: {SELF_TEST_PROMPT}")
}

pub struct CliResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub attachment: Option<AgentSession>,
}

pub fn pane_placement(local_operator_launch: bool, inside_tmux: bool) -> PanePlacement {
    match (local_operator_launch, inside_tmux) {
        (true, true) => PanePlacement::ReuseCurrentPane,
        (true, false) => PanePlacement::DetachedSession { attach: true },
        (false, true) => PanePlacement::DetachedWindow,
        (false, false) => PanePlacement::DetachedSession { attach: false },
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ThreadAction {
    Prompt(String),
    Stop,
    MarkUnavailable,
    Name,
    Rename(String),
    Ignore,
    Reject(String),
}

pub const MAX_SLACK_PHOTO_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackFile {
    pub id: String,
    pub name: String,
    pub mimetype: String,
    pub download_url: String,
    pub size: u64,
}

#[derive(Clone)]
pub struct ThreadMessage<'a> {
    pub channel_id: &'a str,
    pub thread_ts: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub text: &'a str,
    pub has_subtype: bool,
    pub is_bot: bool,
    pub event_ts: Option<&'a str>,
    pub files: Vec<SlackFile>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlAction {
    NewSession {
        agent_cli: String,
        project_name: String,
        agent_args: Vec<String>,
    },
    Ignore,
    Reject(String),
}

#[derive(Clone, Copy)]
pub struct ControlCommand<'a> {
    pub command: &'a str,
    pub channel_id: &'a str,
    pub user_id: &'a str,
    pub text: &'a str,
}

pub trait LocalServices {
    fn available_session_name(&mut self, base: &str) -> Result<String, String>;
    fn create_session_thread(&mut self, session: AgentSession) -> Result<AgentSession, String>;
    fn update_session_thread(&mut self, session: &AgentSession) -> Result<(), String>;
    fn spawn_agent(&mut self, session: AgentSession) -> Result<AgentSession, String>;
    /// Register an already-spawned Agent Session and ensure the Host daemon is running.
    /// Failure leaves the local Agent CLI usable without Slack control.
    fn register_session(&mut self, session: &AgentSession) -> Result<(), String>;
    fn list_sessions(&mut self) -> Result<Vec<AgentSession>, String>;
    fn attach_session(&mut self, session_name: &str) -> Result<(), String>;
    fn cleanup_sessions(&mut self) -> Result<usize, String>;
}

pub fn run_cli_with_services(
    args: &[&str],
    setting_toml: &str,
    env: &BTreeMap<&str, &str>,
    services: &mut dyn LocalServices,
) -> CliResult {
    let Some(command) = args.get(1).copied() else {
        return fail(usage());
    };

    match command {
        "codex" | "grok" => launch_agent(command, &args[2..], setting_toml, env, services),
        "list" | "attach" | "cleanup" => {
            manage_or_defer(command, args, setting_toml, env, services)
        }
        other => fail(format!("unknown command: {other}\n{}", usage())),
    }
}

fn launch_agent(
    agent_cli: &str,
    agent_args: &[&str],
    setting_toml: &str,
    env: &BTreeMap<&str, &str>,
    services: &mut dyn LocalServices,
) -> CliResult {
    let agent_args = match parse_agent_args(agent_args) {
        Ok(agent_args) => agent_args,
        Err(message) => return fail(message),
    };
    let setting = match parse_setting(setting_toml) {
        Ok(setting) => setting,
        Err(message) => {
            return launch_unbridged_agent(agent_cli, agent_args, env, services, message);
        }
    };

    if let Err(message) = require_slack_tokens(env) {
        return launch_unbridged_agent(agent_cli, agent_args, env, services, message);
    }

    let cwd = current_dir(env);
    let project = local_project(&setting, &cwd);
    let self_test = env.contains_key("PINGME_SELF_TEST");
    let status_file = env
        .get("PINGME_STATUS_FILE")
        .filter(|path| !path.is_empty())
        .map(|path| (*path).to_owned());
    if self_test && status_file.is_none() {
        return fail("missing environment variable PINGME_STATUS_FILE\n".to_owned());
    }
    if status_file
        .as_deref()
        .is_some_and(|path| path.contains(['\t', '\n', '\r']))
    {
        return fail("PINGME_STATUS_FILE contains unsupported control characters\n".to_owned());
    }
    let session_name =
        match services.available_session_name(&format!("{agent_cli}-{}", project.name)) {
            Ok(session_name) => session_name,
            Err(error) => return fail(format!("failed to choose Session Name: {error}\n")),
        };
    let mut session = AgentSession::for_launch(
        session_name,
        agent_cli,
        &setting.host.name,
        &project.name,
        cwd,
        &setting.slack.control_channel_id,
        &setting.slack.operator_id,
        pane_placement(true, env.contains_key("TMUX")),
    );
    session.agent_args.clone_from(&agent_args);
    if let Some(status_file) = status_file {
        session.enable_self_test(status_file);
    }

    let session = match services.create_session_thread(session) {
        Ok(session) => session,
        Err(error) => {
            return launch_unbridged_agent(
                agent_cli,
                agent_args,
                env,
                services,
                format!("failed to create Slack Session Thread: {error}\n"),
            );
        }
    };

    let bridge = Bridge::new(&session.control_channel, &session.operator);
    let started = match bridge.start_agent_session(session, services) {
        Ok(started) => started,
        Err(error) => {
            return fail(format!(
                "failed to launch {} CLI: {error}\n",
                agent_display_name(agent_cli)
            ));
        }
    };
    let stderr = format!(
        "{}{}",
        started.status_warning.as_deref().unwrap_or(""),
        started.registration_error.as_deref().unwrap_or("")
    );

    let thread_identity = thread_identity(&started.session);
    CliResult {
        exit_code: 0,
        stdout: format!(
            "Slack Session Thread: {}\nlaunching {} CLI\n",
            thread_identity,
            agent_display_name(agent_cli),
        ),
        stderr,
        attachment: Some(started.session),
    }
}

pub fn render_session_status_card(session: &AgentSession) -> String {
    let status = if session.available {
        session.status
    } else {
        SessionStatus::Unavailable
    };
    format!(
        "PingMe Session\nAgent: {}\nSession: {}\nStatus: {}\nHost: {}\nProject: {}\ncwd: {}\nPane: {}\nThread: {}",
        session.agent_cli,
        session.session_name,
        status.as_str(),
        session.host,
        session.project,
        session.cwd,
        session.pane_id.as_deref().unwrap_or("pending"),
        session.thread_ts.as_deref().unwrap_or("pending"),
    )
}

pub fn thread_action(
    message: &ThreadMessage<'_>,
    control_channel_id: &str,
    thread_ts: &str,
    operator_id: &str,
    agent_cli: &str,
) -> ThreadAction {
    if message.channel_id != control_channel_id || message.thread_ts != Some(thread_ts) {
        return ThreadAction::Ignore;
    }
    if message.is_bot {
        return ThreadAction::Ignore;
    }
    if message.has_subtype && message.files.is_empty() {
        return ThreadAction::Ignore;
    }
    if message.user_id != Some(operator_id) {
        return ThreadAction::Reject(session_update_reject(agent_cli, "unauthorized"));
    }

    let text = message.text.trim();
    if !message.files.is_empty() {
        return match accepted_slack_photos(&message.files) {
            Ok(photos) if photos.is_empty() => ThreadAction::Ignore,
            Ok(_) => ThreadAction::Prompt(text.to_owned()),
            Err(reason) => ThreadAction::Reject(reason),
        };
    }
    match text {
        "pingme stop" => ThreadAction::Stop,
        "pingme name" => ThreadAction::Name,
        _ => {
            if let Some(name) = text.strip_prefix("pingme rename ") {
                let name = name.trim();
                if name.is_empty() {
                    ThreadAction::Reject("usage: pingme rename <new-session-name>".to_owned())
                } else {
                    ThreadAction::Rename(name.to_owned())
                }
            } else {
                ThreadAction::Prompt(text.to_owned())
            }
        }
    }
}

pub fn apply_host_routing_policy(
    message: &ThreadMessage<'_>,
    session: &AgentSession,
    action: ThreadAction,
    started_at: f64,
) -> ThreadAction {
    if matches!(action, ThreadAction::Ignore) {
        return action;
    }
    if message
        .event_ts
        .and_then(|event_ts| event_ts.parse::<f64>().ok())
        .is_none_or(|event_ts| event_ts < started_at)
    {
        return ThreadAction::Reject(session_update_reject(&session.agent_cli, "stale"));
    }
    match action {
        ThreadAction::Prompt(_) if session.available && session.status == SessionStatus::Busy => {
            ThreadAction::Reject(session_update_reject(&session.agent_cli, "busy"))
        }
        ThreadAction::Prompt(_) | ThreadAction::Stop if !session.available => {
            ThreadAction::MarkUnavailable
        }
        action => action,
    }
}

pub fn accepted_slack_photos(files: &[SlackFile]) -> Result<Vec<&SlackFile>, String> {
    let mut photos = Vec::new();
    for file in files {
        if slack_photo_extension(&file.mimetype).is_none() {
            let label = if file.name.is_empty() {
                file.mimetype.as_str()
            } else {
                file.name.as_str()
            };
            return Err(format!("not executed: unsupported file type: {label}"));
        }
        if file.size > MAX_SLACK_PHOTO_BYTES {
            return Err(format!("not executed: photo exceeds 20 MiB: {}", file.name));
        }
        if file.id.is_empty()
            || !file
                .id
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err("not executed: photo is missing a Slack file id".to_owned());
        }
        if !slack_file_url_is_downloadable(&file.download_url) {
            return Err("not executed: photo URL is not a Slack file URL".to_owned());
        }
        photos.push(file);
    }
    Ok(photos)
}

pub fn slack_file_url_is_downloadable(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    host == "files.slack.com" || host.ends_with(".slack.com")
}

pub fn slack_photo_extension(mimetype: &str) -> Option<&'static str> {
    match mimetype.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/heic" => Some("heic"),
        "image/heif" => Some("heif"),
        "image/bmp" => Some("bmp"),
        "image/tiff" | "image/tif" => Some("tiff"),
        _ => None,
    }
}

pub fn session_update_after_prompt_paste(paste: Result<(), String>, agent_cli: &str) -> String {
    match paste {
        Ok(()) => format!("(ping {agent_cli})"),
        Err(error) => format!(
            "Not executed: failed to send prompt to {}: {error}",
            agent_display_name(agent_cli)
        ),
    }
}

pub fn session_update_after_agent_turn(agent_cli: &str, response: &str) -> String {
    format!("(pong {agent_cli})\n{response}")
}

pub fn session_update_reject(agent_cli: &str, reason: &str) -> String {
    format!("(reject {agent_cli} {reason})")
}

pub fn prompt_with_local_photos(text: &str, paths: &[String]) -> String {
    let text = text.trim();
    if paths.is_empty() {
        return text.to_owned();
    }
    let listed = paths.join("\n");
    if text.is_empty() {
        listed
    } else {
        format!("{text}\n{listed}")
    }
}

pub fn slack_photo_store_path(thread_ts: &str, photo: &SlackFile) -> Result<String, String> {
    let extension = slack_photo_extension(&photo.mimetype)
        .ok_or_else(|| "not executed: unsupported file type".to_owned())?;
    if photo.id.is_empty()
        || !photo
            .id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err("not executed: photo is missing a Slack file id".to_owned());
    }
    let safe_thread: String = thread_ts
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '.' {
                character
            } else {
                '_'
            }
        })
        .collect();
    Ok(std::env::temp_dir()
        .join("pingme-photos")
        .join(safe_thread)
        .join(format!("{}.{extension}", photo.id))
        .display()
        .to_string())
}

pub fn control_action(
    command: &ControlCommand<'_>,
    control_channel_id: &str,
    operator_id: &str,
) -> ControlAction {
    if command.command != "/pingme" || command.channel_id != control_channel_id {
        return ControlAction::Ignore;
    }
    if command.user_id != operator_id {
        return ControlAction::Reject("Not executed: unauthorized Slack user.".to_owned());
    }
    let mut words = command.text.split_whitespace();
    let agent_cli = words.next().unwrap_or("").to_owned();
    let project_name = words.next().unwrap_or("").to_owned();
    if agent_cli.is_empty() || project_name.is_empty() {
        return ControlAction::Reject(
            "usage: /pingme <codex|grok> <project> [agent-args...]".to_owned(),
        );
    }
    ControlAction::NewSession {
        agent_cli,
        project_name,
        agent_args: words.map(str::to_owned).collect(),
    }
}

fn show_unbridged_agent(
    services: &mut dyn LocalServices,
    session: AgentSession,
    warning: String,
) -> CliResult {
    let agent_cli = session.agent_cli.clone();
    let session = match services.spawn_agent(session) {
        Ok(session) => session,
        Err(error) => {
            return fail(format!(
                "{warning}failed to launch {} CLI: {error}\n",
                agent_display_name(&agent_cli)
            ));
        }
    };

    CliResult {
        exit_code: 0,
        stdout: format!("launching {} CLI\n", agent_display_name(&session.agent_cli)),
        stderr: format!(
            "{warning}launching unbridged {} CLI; no Slack Thread was created\n",
            agent_display_name(&session.agent_cli)
        ),
        attachment: Some(session),
    }
}

fn thread_identity(session: &AgentSession) -> String {
    session.thread_permalink.clone().unwrap_or_else(|| {
        format!(
            "{} {}",
            session.control_channel,
            session.thread_ts.as_deref().unwrap_or("pending")
        )
    })
}

fn launch_unbridged_agent(
    agent_cli: &str,
    agent_args: Vec<String>,
    env: &BTreeMap<&str, &str>,
    services: &mut dyn LocalServices,
    warning: String,
) -> CliResult {
    let mut session = AgentSession::for_launch(
        services
            .available_session_name(&format!("{agent_cli}-unbridged"))
            .unwrap_or_else(|_| format!("{agent_cli}-unbridged")),
        agent_cli,
        "",
        "unknown",
        current_dir(env),
        "",
        "",
        pane_placement(true, env.contains_key("TMUX")),
    );
    session.agent_args = agent_args;

    show_unbridged_agent(services, session, warning)
}

fn parse_agent_args(args: &[&str]) -> Result<Vec<String>, String> {
    if args.iter().any(|arg| arg.contains(['\t', '\n', '\r'])) {
        Err("Agent CLI arguments contain unsupported control characters\n".to_owned())
    } else {
        Ok(args.iter().map(|arg| (*arg).to_owned()).collect())
    }
}

fn manage_or_defer(
    command: &str,
    args: &[&str],
    setting_toml: &str,
    env: &BTreeMap<&str, &str>,
    services: &mut dyn LocalServices,
) -> CliResult {
    if let Err(message) = require_slack_tokens(env) {
        return fail(message);
    }

    let setting = match parse_setting(setting_toml) {
        Ok(setting) => setting,
        Err(message) => return fail(message),
    };

    match command {
        "list" => match services.list_sessions() {
            Ok(sessions) if sessions.is_empty() => {
                ok(format!("{}no Agent Sessions\n", setting_summary(&setting),))
            }
            Ok(sessions) => ok(format!(
                "{}{}",
                setting_summary(&setting),
                session_list(&sessions)
            )),
            Err(error) => fail(format!("failed to list Agent Sessions: {error}\n")),
        },
        "attach" => match args.get(2) {
            Some(session_name) => match services.attach_session(session_name) {
                Ok(()) => ok(format!("attached: {session_name}\n")),
                Err(error) => fail(format!("failed to attach {session_name}: {error}\n")),
            },
            None => fail(format!("missing session-name\n{}", usage())),
        },
        "cleanup" => match services.cleanup_sessions() {
            Ok(count) => ok(format!("cleaned up {count} unavailable Agent Sessions\n")),
            Err(error) => fail(format!("failed to clean up Agent Sessions: {error}\n")),
        },
        _ => unreachable!("command was filtered by caller"),
    }
}

fn session_list(sessions: &[AgentSession]) -> String {
    let mut output = String::new();
    for session in sessions {
        let status = if session.available {
            session.status
        } else {
            SessionStatus::Unavailable
        };
        output.push_str(&format!(
            "{} {} project={} status={} pane={} thread={}\n",
            session.session_name,
            session.agent_cli,
            session.project,
            status.as_str(),
            session.pane_id.as_deref().unwrap_or("pending"),
            session.thread_ts.as_deref().unwrap_or("none")
        ));
    }
    output
}

fn parse_setting(setting_toml: &str) -> Result<HostSettings, String> {
    if setting_toml.trim().is_empty() {
        return Err("setting.toml is missing or empty\n".to_owned());
    }
    HostSettings::parse(setting_toml).map_err(|error| format!("{error}\n"))
}

fn usage() -> String {
    "usage: pingme <codex|grok> [agent-args...] | pingme <list|attach <session-name>|cleanup>\n"
        .to_owned()
}

pub fn agent_display_name(agent_cli: &str) -> &str {
    match agent_cli {
        "codex" => "Codex",
        "grok" => "Grok",
        _ => agent_cli,
    }
}

fn ok(stdout: String) -> CliResult {
    CliResult {
        exit_code: 0,
        stdout,
        stderr: String::new(),
        attachment: None,
    }
}

fn fail(stderr: String) -> CliResult {
    CliResult {
        exit_code: 2,
        stdout: String::new(),
        stderr,
        attachment: None,
    }
}

fn require_slack_tokens(env: &BTreeMap<&str, &str>) -> Result<(), String> {
    let mut missing = Vec::new();
    for name in ["SLACK_APP_TOKEN", "SLACK_BOT_TOKEN"] {
        if env.get(name).is_none_or(|value| value.is_empty()) {
            missing.push(format!("missing environment variable {name}"));
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{}\n", missing.join("\n")))
    }
}

fn current_dir(env: &BTreeMap<&str, &str>) -> String {
    env.get("PWD").copied().unwrap_or(".").to_owned()
}

fn local_project(setting: &HostSettings, cwd: &str) -> ProjectSetting {
    setting
        .projects
        .iter()
        .find(|project| project.cwd == cwd)
        .cloned()
        .unwrap_or(ProjectSetting {
            name: "unknown".to_owned(),
            cwd: cwd.to_owned(),
        })
}

fn setting_summary(setting: &HostSettings) -> String {
    let mut output = format!(
        "Host: {}\nControl Channel: {}\nOperator: {}\n",
        setting.host.name, setting.slack.control_channel_id, setting.slack.operator_id,
    );
    for project in &setting.projects {
        output.push_str(&format!("Project: {} -> {}\n", project.name, project.cwd));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTING: &str = r#"
[host]
name = "linux"

[slack]
control_channel_id = "C_TEST_CONTROL"
operator_id = "U_TEST_OPERATOR"

[[projects]]
name = "pingme"
cwd = "/work/pingme"
"#;

    #[derive(Default)]
    struct FakeServices {
        calls: Vec<&'static str>,
        slack_error: Option<String>,
        update_error: Option<String>,
        daemon_error: Option<String>,
        without_permalink: bool,
        updated_card: Option<String>,
        registered_session: Option<AgentSession>,
        spawned_session: Option<AgentSession>,
        created_session: Option<AgentSession>,
        next_session_name: Option<String>,
    }

    impl LocalServices for FakeServices {
        fn available_session_name(&mut self, base: &str) -> Result<String, String> {
            Ok(self
                .next_session_name
                .clone()
                .unwrap_or_else(|| base.to_owned()))
        }

        fn create_session_thread(&mut self, session: AgentSession) -> Result<AgentSession, String> {
            self.calls.push("create_session_thread");
            self.created_session = Some(session.clone());
            match &self.slack_error {
                Some(error) => Err(error.clone()),
                None => {
                    let mut session = session;
                    session.thread_ts = Some("1757221923.123456".to_owned());
                    session.thread_permalink = if self.without_permalink {
                        None
                    } else {
                        Some("https://workspace.slack.com/archives/C/thread".to_owned())
                    };
                    Ok(session)
                }
            }
        }

        fn update_session_thread(&mut self, session: &AgentSession) -> Result<(), String> {
            self.calls.push("update_session_thread");
            assert_eq!("C_TEST_CONTROL", session.control_channel);
            assert_eq!(Some("1757221923.123456"), session.thread_ts.as_deref());
            self.updated_card = Some(render_session_status_card(session));
            match &self.update_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn spawn_agent(&mut self, session: AgentSession) -> Result<AgentSession, String> {
            self.calls.push("spawn_agent");
            self.spawned_session = Some(session.clone());
            let tmux_session = matches!(
                session.placement,
                Some(PanePlacement::DetachedSession { .. })
            )
            .then(|| session.session_name.clone());
            let mut session = session;
            session.pane_id = Some("%7".to_owned());
            session.tmux_session = tmux_session;
            session.notify_socket = Some("/tmp/pingme-test.sock".to_owned());
            session.notify_token = Some("notify-token".to_owned());
            Ok(session)
        }

        fn register_session(&mut self, session: &AgentSession) -> Result<(), String> {
            self.calls.push("register_session");
            self.registered_session = Some(session.clone());
            match &self.daemon_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn list_sessions(&mut self) -> Result<Vec<AgentSession>, String> {
            Ok(Vec::new())
        }

        fn attach_session(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }

        fn cleanup_sessions(&mut self) -> Result<usize, String> {
            self.calls.push("cleanup_sessions");
            Ok(2)
        }
    }

    fn env_with_tokens() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            ("SLACK_APP_TOKEN", "xapp-test"),
            ("SLACK_BOT_TOKEN", "xoxb-test"),
            ("PWD", "/work/pingme"),
        ])
    }

    fn run_cli(args: &[&str], setting_toml: &str, env: &BTreeMap<&str, &str>) -> CliResult {
        run_cli_with_services(args, setting_toml, env, &mut FakeServices::default())
    }

    fn listed_session(session_name: &str, pane_id: &str, thread_ts: &str) -> AgentSession {
        let mut session = AgentSession::for_launch(
            session_name,
            "codex",
            "linux",
            "pingme",
            "/work/pingme",
            "C_TEST_CONTROL",
            "U_TEST_OPERATOR",
            PanePlacement::DetachedWindow,
        );
        session.thread_ts = Some(thread_ts.to_owned());
        session.pane_id = Some(pane_id.to_owned());
        session.tmux_session = Some("bridge".to_owned());
        session.tmux_window = Some(format!("bridge:{session_name}"));
        session.set_status(SessionStatus::Idle);
        session
    }

    #[test]
    fn host_bridge_routes_each_session_thread_to_its_own_agent_session() {
        let bridge = Bridge::new("C_TEST_CONTROL", "U_TEST_OPERATOR");
        let sessions = vec![
            listed_session("first", "%1", "100.1"),
            listed_session("second", "%2", "100.2"),
        ];
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("100.2"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "continue",
            has_subtype: false,
            is_bot: false,
            event_ts: Some("101.0"),
            files: Vec::new(),
        };

        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(Some("%2"), routed.session.pane_id.as_deref());
        assert_eq!(ThreadAction::Prompt("continue".to_owned()), routed.action);
    }

    #[test]
    fn host_bridge_routes_each_notification_token_to_its_own_agent_session() {
        let bridge = Bridge::new("C_TEST_CONTROL", "U_TEST_OPERATOR");
        let mut first = listed_session("first", "%1", "100.1");
        first.notify_token = Some("first-token".to_owned());
        let mut second = listed_session("second", "%2", "100.2");
        second.notify_token = Some("second-token".to_owned());

        let sessions = [first, second];
        let session = bridge
            .route_notification("second-token", &sessions)
            .unwrap();

        assert_eq!(Some("%2"), session.pane_id.as_deref());
    }

    #[test]
    fn self_test_bot_message_routes_through_its_session_thread() {
        let bridge = Bridge::new("C_TEST_CONTROL", "U_TEST_OPERATOR");
        let mut session = listed_session("self-test", "%1", "100.1");
        session.notify_token = Some("self-test-token".to_owned());
        session.enable_self_test("/tmp/self-test.status".to_owned());
        let text = self_test_message("self-test-token");
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("100.1"),
            user_id: None,
            text: &text,
            has_subtype: true,
            is_bot: true,
            event_ts: Some("101.0"),
            files: Vec::new(),
        };

        let sessions = [session];
        let routed = bridge.route_thread_message(&message, &sessions).unwrap();

        assert_eq!(
            ThreadAction::Prompt("Respond with exactly roundtrip-ok".to_owned()),
            routed.action
        );

        let wrong_text = self_test_message("wrong-token");
        assert_eq!(
            ThreadAction::Ignore,
            bridge
                .route_thread_message(
                    &ThreadMessage {
                        text: &wrong_text,
                        ..message.clone()
                    },
                    &sessions,
                )
                .unwrap()
                .action
        );
    }

    #[test]
    fn self_test_launch_requires_and_registers_a_status_file() {
        let env = BTreeMap::from([
            ("SLACK_APP_TOKEN", "xapp-test"),
            ("SLACK_BOT_TOKEN", "xoxb-test"),
            ("PWD", "/work/pingme"),
            ("PINGME_SELF_TEST", "1"),
            ("PINGME_STATUS_FILE", "/tmp/pingme.status"),
        ]);
        let mut services = FakeServices::default();

        let result = run_cli_with_services(&["pingme", "codex"], SETTING, &env, &mut services);

        assert_eq!(0, result.exit_code);
        let registration = services.registered_session.unwrap();
        assert!(registration.self_test);
        assert_eq!(
            Some("/tmp/pingme.status"),
            registration.status_file.as_deref()
        );
    }

    #[test]
    fn local_launch_inside_tmux_reuses_the_current_pane() {
        assert_eq!(PanePlacement::ReuseCurrentPane, pane_placement(true, true));

        let mut env = env_with_tokens();
        env.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        env.insert("TMUX_PANE", "%7");
        for agent in ["codex", "grok"] {
            let mut services = FakeServices::default();
            let result = run_cli_with_services(&["pingme", agent], SETTING, &env, &mut services);

            assert_eq!(0, result.exit_code);
            assert_eq!(
                Some(PanePlacement::ReuseCurrentPane),
                result
                    .attachment
                    .as_ref()
                    .and_then(|session| session.placement)
            );
            assert_eq!(
                Some(PanePlacement::ReuseCurrentPane),
                services
                    .spawned_session
                    .as_ref()
                    .and_then(|session| session.placement)
            );
        }
    }

    #[test]
    fn local_launch_outside_tmux_attaches_the_new_session() {
        assert_eq!(
            PanePlacement::DetachedSession { attach: true },
            pane_placement(true, false)
        );

        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            Some(PanePlacement::DetachedSession { attach: true }),
            result
                .attachment
                .as_ref()
                .and_then(|session| session.placement)
        );
        assert_eq!(
            Some(PanePlacement::DetachedSession { attach: true }),
            services
                .spawned_session
                .as_ref()
                .and_then(|session| session.placement)
        );
    }

    #[test]
    fn unbridged_local_launch_inside_tmux_reuses_the_current_pane() {
        let env = BTreeMap::from([("TMUX", "/tmp/tmux-1000/default,1,0"), ("TMUX_PANE", "%7")]);
        let mut services = FakeServices::default();
        let result = run_cli_with_services(&["pingme", "codex"], SETTING, &env, &mut services);

        assert_eq!(0, result.exit_code);
        let session = result.attachment.unwrap();
        assert_eq!(Some(PanePlacement::ReuseCurrentPane), session.placement);
        assert_eq!(None, session.thread_ts);
    }

    #[test]
    fn accepts_local_launch_commands() {
        let codex = run_cli(&["pingme", "codex"], SETTING, &env_with_tokens());
        let grok = run_cli(&["pingme", "grok"], SETTING, &env_with_tokens());

        assert_eq!(0, codex.exit_code);
        assert!(codex.stdout.contains("launching Codex CLI"));
        assert_eq!(0, grok.exit_code);
        assert!(grok.stdout.contains("launching Grok CLI"));
    }

    #[test]
    fn local_codex_creates_slack_thread_before_launching_codex() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            Some(PanePlacement::DetachedSession { attach: true }),
            result
                .attachment
                .as_ref()
                .and_then(|session| session.placement)
        );
        assert_eq!(
            vec![
                "create_session_thread",
                "spawn_agent",
                "update_session_thread",
                "register_session"
            ],
            services.calls
        );
        assert_eq!(
            "codex-pingme",
            services.created_session.as_ref().unwrap().session_name
        );
        assert_eq!(
            "codex-pingme",
            services.spawned_session.as_ref().unwrap().session_name
        );
        assert_eq!(
            "/work/pingme",
            services.spawned_session.as_ref().unwrap().cwd
        );
        assert_eq!(
            Some("1757221923.123456"),
            services
                .spawned_session
                .as_ref()
                .unwrap()
                .thread_ts
                .as_deref()
        );
        assert!(
            result
                .stdout
                .contains("https://workspace.slack.com/archives/C/thread")
        );
        let card = services.updated_card.unwrap();
        assert!(card.contains("Agent: codex"));
        assert!(card.contains("Session: codex-pingme"));
        assert!(card.contains("Status: idle"));
        assert!(card.contains("Host: linux"));
        assert!(card.contains("Project: pingme"));
        assert!(card.contains("cwd: /work/pingme"));
        assert!(card.contains("Pane: %7"));
        assert!(card.contains("Thread: 1757221923.123456"));
        let registration = services.registered_session.unwrap();
        assert_eq!("C_TEST_CONTROL", registration.control_channel);
        assert_eq!("U_TEST_OPERATOR", registration.operator);
        assert_eq!(Some("1757221923.123456"), registration.thread_ts.as_deref());
        assert_eq!("codex-pingme", registration.session_name);
        assert_eq!("linux", registration.host);
        assert_eq!("pingme", registration.project);
        assert_eq!("/work/pingme", registration.cwd);
        assert_eq!(Some("%7"), registration.pane_id.as_deref());
        assert_eq!(
            Some("/tmp/pingme-test.sock"),
            registration.notify_socket.as_deref()
        );
        assert_eq!(Some("notify-token"), registration.notify_token.as_deref());
    }

    #[test]
    fn local_grok_uses_the_same_slack_session_flow() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &["pingme", "grok"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            vec![
                "create_session_thread",
                "spawn_agent",
                "update_session_thread",
                "register_session"
            ],
            services.calls
        );
        assert_eq!("grok", services.created_session.as_ref().unwrap().agent_cli);
        assert_eq!(
            "grok-pingme",
            services.spawned_session.as_ref().unwrap().session_name
        );
        assert_eq!(
            "grok",
            services.registered_session.as_ref().unwrap().agent_cli
        );
        assert!(result.stdout.contains("launching Grok CLI"));
    }

    #[test]
    fn local_launch_uses_an_available_session_name() {
        let mut services = FakeServices {
            next_session_name: Some("codex-pingme-2".to_owned()),
            ..FakeServices::default()
        };

        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            "codex-pingme-2",
            services.created_session.unwrap().session_name
        );
    }

    #[test]
    fn remote_agent_session_start_does_not_attach_a_local_terminal() {
        assert_eq!(PanePlacement::DetachedWindow, pane_placement(false, true));
        assert_eq!(
            PanePlacement::DetachedSession { attach: false },
            pane_placement(false, false)
        );

        let bridge = Bridge::new("C_TEST_CONTROL", "U_TEST_OPERATOR");

        for placement in [
            PanePlacement::DetachedWindow,
            PanePlacement::DetachedSession { attach: false },
        ] {
            let mut session = AgentSession::for_launch(
                "codex-pingme",
                "codex",
                "linux",
                "pingme",
                "/work/pingme",
                "C_TEST_CONTROL",
                "U_TEST_OPERATOR",
                placement,
            );
            session.thread_ts = Some("1757221923.123456".to_owned());
            let mut services = FakeServices::default();
            let started = bridge.start_agent_session(session, &mut services).unwrap();

            assert_eq!(
                vec!["spawn_agent", "update_session_thread", "register_session"],
                services.calls
            );
            assert_eq!(Some(placement), services.spawned_session.unwrap().placement);
            assert_eq!(Some(placement), started.session.placement);
            assert_ne!(
                Some(PanePlacement::ReuseCurrentPane),
                started.session.placement
            );
        }
    }

    #[test]
    fn local_launch_output_falls_back_to_channel_and_thread_ts() {
        let mut services = FakeServices {
            without_permalink: true,
            ..FakeServices::default()
        };
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert!(result.stdout.contains("C_TEST_CONTROL 1757221923.123456"));
    }

    #[test]
    fn preserves_status_update_warning_with_launched_session() {
        let mut services = FakeServices {
            update_error: Some("cant_update".to_owned()),
            ..FakeServices::default()
        };
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert!(result.stderr.contains("cant_update"));
        assert!(result.attachment.is_some());
    }

    #[test]
    fn renders_status_cards_for_session_states() {
        let launch = AgentSession::for_launch(
            "codex-unknown",
            "codex",
            "linux",
            "unknown",
            "/tmp/worktree",
            "C_TEST_CONTROL",
            "U_TEST_OPERATOR",
            PanePlacement::DetachedWindow,
        );

        let starting = render_session_status_card(&launch);
        let mut session = launch;
        session.thread_ts = Some("1757221923.123456".to_owned());
        session.pane_id = Some("%7".to_owned());
        session.set_status(SessionStatus::Idle);
        let idle = render_session_status_card(&session);
        session.set_status(SessionStatus::Busy);
        let busy = render_session_status_card(&session);
        session.set_status(SessionStatus::Unavailable);
        let unavailable = render_session_status_card(&session);

        assert!(starting.contains("Status: starting"));
        assert!(starting.contains("Project: unknown"));
        assert!(starting.contains("cwd: /tmp/worktree"));
        assert!(starting.contains("Pane: pending"));
        assert!(starting.contains("Thread: pending"));
        assert!(idle.contains("Status: idle"));
        assert!(idle.contains("Pane: %7"));
        assert!(idle.contains("Thread: 1757221923.123456"));
        assert!(busy.contains("Status: busy"));
        assert!(unavailable.contains("Status: unavailable"));
    }

    #[test]
    fn updates_same_root_message_for_status_changes() {
        let mut session = listed_session("codex-pingme", "%7", "1757221923.123456");
        let mut services = FakeServices::default();

        for status in [
            SessionStatus::Starting,
            SessionStatus::Idle,
            SessionStatus::Busy,
            SessionStatus::Unavailable,
        ] {
            session.set_status(status);
            services.update_session_thread(&session).unwrap();
        }

        assert_eq!(
            vec![
                "update_session_thread",
                "update_session_thread",
                "update_session_thread",
                "update_session_thread"
            ],
            services.calls
        );
    }

    #[test]
    fn routes_only_operator_messages_in_mapped_thread() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "hello codex",
            has_subtype: false,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: Vec::new(),
        };

        assert_eq!(
            ThreadAction::Prompt("hello codex".to_owned()),
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );

        let wrong_thread = ThreadMessage {
            thread_ts: Some("other"),
            ..message.clone()
        };
        assert_eq!(
            ThreadAction::Ignore,
            thread_action(
                &wrong_thread,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );

        let wrong_user = ThreadMessage {
            user_id: Some("UOTHER"),
            ..message.clone()
        };
        assert_eq!(
            ThreadAction::Reject(session_update_reject("codex", "unauthorized")),
            thread_action(
                &wrong_user,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );

        let bot_message = ThreadMessage {
            has_subtype: true,
            ..message.clone()
        };
        assert_eq!(
            ThreadAction::Ignore,
            thread_action(
                &bot_message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );

        let bot_without_subtype = ThreadMessage {
            user_id: Some("UBOT"),
            text: "not executed: unauthorized Slack user",
            is_bot: true,
            ..message.clone()
        };
        assert_eq!(
            ThreadAction::Ignore,
            thread_action(
                &bot_without_subtype,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn recognizes_thread_controls_exactly() {
        let base = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "",
            has_subtype: false,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: Vec::new(),
        };

        assert_eq!(
            ThreadAction::Stop,
            thread_action(
                &ThreadMessage {
                    text: "pingme stop",
                    ..base.clone()
                },
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
        assert_eq!(
            ThreadAction::Name,
            thread_action(
                &ThreadMessage {
                    text: "pingme name",
                    ..base.clone()
                },
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
        assert_eq!(
            ThreadAction::Rename("better-name".to_owned()),
            thread_action(
                &ThreadMessage {
                    text: "pingme rename better-name",
                    ..base.clone()
                },
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
        assert_eq!(
            ThreadAction::Prompt("please write about pingme stop".to_owned()),
            thread_action(
                &ThreadMessage {
                    text: "please write about pingme stop",
                    ..base.clone()
                },
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
        assert_eq!(
            ThreadAction::Prompt("cli-stop".to_owned()),
            thread_action(
                &ThreadMessage {
                    text: "cli-stop",
                    ..base.clone()
                },
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    fn slack_photo(id: &str, name: &str, mimetype: &str) -> SlackFile {
        SlackFile {
            id: id.to_owned(),
            name: name.to_owned(),
            mimetype: mimetype.to_owned(),
            download_url: format!("https://files.slack.com/files-pri/T/{id}/{name}"),
            size: 1024,
        }
    }

    #[test]
    fn operator_png_file_share_is_a_prompt() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "",
            has_subtype: true,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: vec![slack_photo("F123PNG", "shot.png", "image/png")],
        };

        assert_eq!(
            ThreadAction::Prompt(String::new()),
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn operator_jpeg_caption_is_a_prompt() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "what is wrong here",
            has_subtype: false,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: vec![slack_photo("F123JPG", "photo.jpg", "image/jpeg")],
        };

        assert_eq!(
            ThreadAction::Prompt("what is wrong here".to_owned()),
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn operator_pdf_in_session_thread_is_rejected() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "read this",
            has_subtype: true,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: vec![slack_photo("F123PDF", "notes.pdf", "application/pdf")],
        };

        assert_eq!(
            ThreadAction::Reject("not executed: unsupported file type: notes.pdf".to_owned()),
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn file_share_without_files_is_ignored() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_TEST_OPERATOR"),
            text: "",
            has_subtype: true,
            is_bot: false,
            event_ts: Some("1757221924.000000"),
            files: Vec::new(),
        };

        assert_eq!(
            ThreadAction::Ignore,
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn prompt_with_photos_appends_local_paths() {
        assert_eq!(
            "/tmp/pingme-photos/1757221923.123456/F123PNG.png",
            prompt_with_local_photos(
                "",
                &["/tmp/pingme-photos/1757221923.123456/F123PNG.png".to_owned()]
            )
        );
        assert_eq!(
            "what is wrong here\n/tmp/a.png\n/tmp/b.jpg",
            prompt_with_local_photos(
                "what is wrong here",
                &["/tmp/a.png".to_owned(), "/tmp/b.jpg".to_owned()]
            )
        );
    }

    #[test]
    fn successful_prompt_paste_posts_received_session_update() {
        assert_eq!(
            "(ping codex)",
            session_update_after_prompt_paste(Ok(()), "codex")
        );
        assert_eq!(
            "(ping grok)",
            session_update_after_prompt_paste(Ok(()), "grok")
        );
    }

    #[test]
    fn agent_turn_posts_pong_session_update() {
        assert_eq!(
            "(pong grok)\nhello",
            session_update_after_agent_turn("grok", "hello")
        );
        assert_eq!(
            "(pong codex)\nhello",
            session_update_after_agent_turn("codex", "hello")
        );
    }

    #[test]
    fn policy_rejects_post_lisp_session_updates() {
        assert_eq!("(reject grok busy)", session_update_reject("grok", "busy"));
        assert_eq!(
            "(reject grok stale)",
            session_update_reject("grok", "stale")
        );
        assert_eq!(
            "(reject grok unauthorized)",
            session_update_reject("grok", "unauthorized")
        );
        assert_eq!(
            "(reject grok unavailable)",
            session_update_reject("grok", "unavailable")
        );
        assert_eq!(
            "(reject codex busy)",
            session_update_reject("codex", "busy")
        );
    }

    #[test]
    fn failed_prompt_paste_posts_not_executed_without_received_update() {
        let update = session_update_after_prompt_paste(Err("pane is dead".to_owned()), "codex");
        assert_eq!(
            "Not executed: failed to send prompt to Codex: pane is dead",
            update
        );
        assert!(!update.contains("(ping"));
    }

    #[test]
    fn received_session_update_from_the_bot_is_ignored() {
        let message = ThreadMessage {
            channel_id: "C_TEST_CONTROL",
            thread_ts: Some("1757221923.123456"),
            user_id: Some("U_BRIDGE_BOT"),
            text: "(ping grok)",
            has_subtype: false,
            is_bot: true,
            event_ts: Some("1757221925.000000"),
            files: Vec::new(),
        };

        assert_eq!(
            ThreadAction::Ignore,
            thread_action(
                &message,
                "C_TEST_CONTROL",
                "1757221923.123456",
                "U_TEST_OPERATOR",
                "codex",
            )
        );
    }

    #[test]
    fn photo_store_path_uses_file_id_and_extension() {
        let path = slack_photo_store_path(
            "1757221923.123456",
            &slack_photo("F123PNG", "shot.png", "image/png"),
        )
        .unwrap();
        assert!(
            path.ends_with("pingme-photos/1757221923.123456/F123PNG.png"),
            "{path}"
        );
    }

    #[test]
    fn recognizes_pingme_control_command() {
        let command = ControlCommand {
            command: "/pingme",
            channel_id: "C_TEST_CONTROL",
            user_id: "U_TEST_OPERATOR",
            text: "codex pingme",
        };

        assert_eq!(
            ControlAction::NewSession {
                agent_cli: "codex".to_owned(),
                project_name: "pingme".to_owned(),
                agent_args: Vec::new(),
            },
            control_action(&command, "C_TEST_CONTROL", "U_TEST_OPERATOR")
        );
        assert_eq!(
            ControlAction::Reject("Not executed: unauthorized Slack user.".to_owned()),
            control_action(
                &ControlCommand {
                    user_id: "UOTHER",
                    ..command
                },
                "C_TEST_CONTROL",
                "U_TEST_OPERATOR",
            )
        );
        assert_eq!(
            ControlAction::Ignore,
            control_action(
                &ControlCommand {
                    channel_id: "COTHER",
                    ..command
                },
                "C_TEST_CONTROL",
                "U_TEST_OPERATOR",
            )
        );
        assert_eq!(
            ControlAction::Ignore,
            control_action(
                &ControlCommand {
                    command: "/cli-new",
                    ..command
                },
                "C_TEST_CONTROL",
                "U_TEST_OPERATOR",
            )
        );
    }

    #[test]
    fn local_codex_falls_back_to_unbridged_when_slack_thread_creation_fails() {
        let mut services = FakeServices {
            slack_error: Some("not_in_channel".to_owned()),
            ..FakeServices::default()
        };
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(vec!["create_session_thread", "spawn_agent"], services.calls);
        assert_eq!(None, services.spawned_session.unwrap().thread_ts);
        assert!(
            result
                .stderr
                .contains("failed to create Slack Session Thread: not_in_channel")
        );
        assert!(result.stderr.contains("launching unbridged Codex CLI"));
    }

    #[test]
    fn local_codex_falls_back_to_unbridged_when_slack_tokens_are_missing() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &["pingme", "codex"],
            SETTING,
            &BTreeMap::new(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(vec!["spawn_agent"], services.calls);
        assert_eq!(None, services.spawned_session.unwrap().thread_ts);
        assert!(
            result
                .stderr
                .contains("missing environment variable SLACK_APP_TOKEN")
        );
        assert!(result.stderr.contains("launching unbridged Codex CLI"));
    }

    #[test]
    fn accepts_local_management_commands() {
        let mut services = FakeServices::default();
        let list = run_cli_with_services(
            &["pingme", "list"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );
        let attach = run_cli_with_services(
            &["pingme", "attach", "codex-pingme-1420"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );
        let cleanup = run_cli_with_services(
            &["pingme", "cleanup"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, list.exit_code);
        assert!(list.stdout.contains("no Agent Sessions"));
        assert_eq!(0, attach.exit_code);
        assert!(attach.stdout.contains("attached: codex-pingme-1420"));
        assert_eq!(0, cleanup.exit_code);
        assert!(
            cleanup
                .stdout
                .contains("cleaned up 2 unavailable Agent Sessions")
        );
        assert!(services.calls.contains(&"cleanup_sessions"));
    }

    #[test]
    fn parses_setting_toml() {
        let result = run_cli(&["pingme", "list"], SETTING, &env_with_tokens());

        assert_eq!(0, result.exit_code);
        assert!(result.stdout.contains("Host: linux"));
        assert!(result.stdout.contains("Control Channel: C_TEST_CONTROL"));
        assert!(result.stdout.contains("Project: pingme -> /work/pingme"));
    }

    #[test]
    fn reports_missing_slack_tokens_without_leaking_values() {
        let result = run_cli(&["pingme", "list"], SETTING, &BTreeMap::new());

        assert_eq!(2, result.exit_code);
        assert!(
            result
                .stderr
                .contains("missing environment variable SLACK_APP_TOKEN")
        );
        assert!(
            result
                .stderr
                .contains("missing environment variable SLACK_BOT_TOKEN")
        );
        assert!(!result.stderr.contains("xapp-"));
        assert!(!result.stderr.contains("xoxb-"));
    }

    #[test]
    fn local_codex_falls_back_to_unbridged_when_setting_toml_is_missing() {
        let mut services = FakeServices::default();
        let result =
            run_cli_with_services(&["pingme", "codex"], "", &env_with_tokens(), &mut services);

        assert_eq!(0, result.exit_code);
        assert_eq!(vec!["spawn_agent"], services.calls);
        assert_eq!(None, services.spawned_session.unwrap().thread_ts);
        assert!(result.stderr.contains("setting.toml is missing or empty"));
        assert!(result.stderr.contains("launching unbridged Codex CLI"));
    }

    #[test]
    fn reports_missing_setting_toml_for_management_commands() {
        let result = run_cli(&["pingme", "list"], "", &env_with_tokens());

        assert_eq!(2, result.exit_code);
        assert!(result.stderr.contains("setting.toml is missing or empty"));
    }

    #[test]
    fn reports_unknown_commands_with_usage() {
        let result = run_cli(&["pingme", "wat"], SETTING, &env_with_tokens());

        assert_eq!(2, result.exit_code);
        assert!(result.stderr.contains("unknown command: wat"));
        assert!(result.stderr.contains("usage: pingme"));
    }

    #[test]
    fn local_codex_forwards_resume_args_to_the_spawned_agent() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &["pingme", "codex", "resume", "session-id"],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            vec!["resume".to_owned(), "session-id".to_owned()],
            services.spawned_session.unwrap().agent_args
        );
    }

    #[test]
    fn local_grok_forwards_resume_args_to_the_spawned_agent() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &[
                "pingme",
                "grok",
                "--resume",
                "01a090da-d3ba-7bb1-8cc0-aab11beccab6",
            ],
            SETTING,
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            vec![
                "--resume".to_owned(),
                "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned()
            ],
            services.spawned_session.unwrap().agent_args
        );
        assert_eq!(
            vec![
                "--resume".to_owned(),
                "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned()
            ],
            result.attachment.unwrap().agent_args
        );
    }

    #[test]
    fn unbridged_local_grok_forwards_resume_args() {
        let mut services = FakeServices::default();
        let result = run_cli_with_services(
            &[
                "pingme",
                "grok",
                "--resume",
                "01a090da-d3ba-7bb1-8cc0-aab11beccab6",
            ],
            "",
            &env_with_tokens(),
            &mut services,
        );

        assert_eq!(0, result.exit_code);
        assert_eq!(
            vec![
                "--resume".to_owned(),
                "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned()
            ],
            services.spawned_session.unwrap().agent_args
        );
    }

    #[test]
    fn rejects_agent_args_with_control_characters() {
        let result = run_cli(
            &["pingme", "grok", "--resume", "id\nnext"],
            SETTING,
            &env_with_tokens(),
        );

        assert_eq!(2, result.exit_code);
        assert!(
            result
                .stderr
                .contains("Agent CLI arguments contain unsupported control characters")
        );
    }

    #[test]
    fn pingme_slash_forwards_codex_resume_after_the_project() {
        let command = ControlCommand {
            command: "/pingme",
            channel_id: "C_TEST_CONTROL",
            user_id: "U_TEST_OPERATOR",
            text: "codex pingme resume session-id",
        };

        assert_eq!(
            ControlAction::NewSession {
                agent_cli: "codex".to_owned(),
                project_name: "pingme".to_owned(),
                agent_args: vec!["resume".to_owned(), "session-id".to_owned()],
            },
            control_action(&command, "C_TEST_CONTROL", "U_TEST_OPERATOR")
        );
    }

    #[test]
    fn pingme_slash_forwards_agent_args_after_the_project() {
        let command = ControlCommand {
            command: "/pingme",
            channel_id: "C_TEST_CONTROL",
            user_id: "U_TEST_OPERATOR",
            text: "grok pingme --resume 01a090da-d3ba-7bb1-8cc0-aab11beccab6",
        };

        assert_eq!(
            ControlAction::NewSession {
                agent_cli: "grok".to_owned(),
                project_name: "pingme".to_owned(),
                agent_args: vec![
                    "--resume".to_owned(),
                    "01a090da-d3ba-7bb1-8cc0-aab11beccab6".to_owned()
                ],
            },
            control_action(&command, "C_TEST_CONTROL", "U_TEST_OPERATOR")
        );
    }
}
