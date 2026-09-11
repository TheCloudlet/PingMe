use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

const TMUX_REGISTRY_FORMAT: &str = concat!(
    "pane_id=#{pane_id}\t",
    "tmux_session=#{session_name}\t",
    "tmux_window=#{session_name}:#{window_index}\t",
    "pane_dead=#{pane_dead}\t",
    "session_name=#{@pingme_session_name}\t",
    "agent_cli=#{@pingme_agent_cli}\t",
    "host=#{@pingme_host}\t",
    "project=#{@pingme_project}\t",
    "cwd=#{@pingme_cwd}\t",
    "status=#{@pingme_status}\t",
    "thread_ts=#{@pingme_thread_ts}\t",
    "thread_permalink=#{@pingme_thread_permalink}\t",
    "control_channel=#{@pingme_channel}\t",
    "operator=#{@pingme_operator}\t",
    "notify_socket=#{@pingme_notify_socket}\t",
    "notify_token=#{@pingme_notify_token}\t",
    "self_test=#{@pingme_self_test}\t",
    "status_file=#{@pingme_status_file}\t",
    "last_slack_prompt=#{@pingme_last_slack_prompt_fingerprint}\t",
    "placement=#{@pingme_placement}",
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanePlacement {
    ReuseCurrentPane,
    DetachedWindow,
    DetachedSession { attach: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Idle,
    Busy,
    Unavailable,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub session_name: String,
    pub agent_cli: String,
    /// Launch-only Agent CLI arguments. Not stored in the live tmux registry.
    pub agent_args: Vec<String>,
    pub host: String,
    pub project: String,
    pub cwd: String,
    pub control_channel: String,
    pub operator: String,
    /// Registry-stored status. Dead panes may still store Idle; cards treat that as Unavailable.
    pub status: SessionStatus,
    pub placement: Option<PanePlacement>,
    pub thread_ts: Option<String>,
    pub thread_permalink: Option<String>,
    pub pane_id: Option<String>,
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub notify_socket: Option<String>,
    pub notify_token: Option<String>,
    pub status_file: Option<String>,
    pub last_slack_prompt: Option<String>,
    /// Live pane. False means cards and Busy routing treat the Session as Unavailable.
    pub available: bool,
    pub self_test: bool,
}

#[derive(Clone, Deserialize)]
pub struct HostSettings {
    pub host: HostSetting,
    pub slack: SlackSetting,
    #[serde(default)]
    pub projects: Vec<ProjectSetting>,
}

#[derive(Clone, Deserialize)]
pub struct HostSetting {
    pub name: String,
}

#[derive(Clone, Deserialize)]
pub struct SlackSetting {
    pub control_channel_id: String,
    pub operator_id: String,
}

#[derive(Clone, Deserialize)]
pub struct ProjectSetting {
    pub name: String,
    pub cwd: String,
}

impl HostSettings {
    pub fn parse(toml: &str) -> Result<Self, String> {
        toml::from_str(toml).map_err(|error| format!("failed to parse setting.toml: {error}"))
    }
}

impl AgentSession {
    #[allow(clippy::too_many_arguments)]
    pub fn for_launch(
        session_name: impl Into<String>,
        agent_cli: impl Into<String>,
        host: impl Into<String>,
        project: impl Into<String>,
        cwd: impl Into<String>,
        control_channel: impl Into<String>,
        operator: impl Into<String>,
        placement: PanePlacement,
    ) -> Self {
        Self {
            session_name: session_name.into(),
            agent_cli: agent_cli.into(),
            agent_args: Vec::new(),
            host: host.into(),
            project: project.into(),
            cwd: cwd.into(),
            control_channel: control_channel.into(),
            operator: operator.into(),
            status: SessionStatus::Starting,
            placement: Some(placement),
            thread_ts: None,
            thread_permalink: None,
            pane_id: None,
            tmux_session: None,
            tmux_window: None,
            notify_socket: None,
            notify_token: None,
            status_file: None,
            last_slack_prompt: None,
            available: true,
            self_test: false,
        }
    }

    pub fn from_registry_record(record: &str) -> Result<Option<Self>, String> {
        let mut fields = BTreeMap::new();
        for field in record.split('\t') {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| format!("registry field has no name: {field}"))?;
            fields.insert(name, value);
        }
        let Some(session_name) = optional_field(&fields, "session_name") else {
            return Ok(None);
        };
        let pane_id = optional_field(&fields, "pane_id")
            .ok_or_else(|| format!("Agent Session {session_name} has no pane_id"))?;
        let agent_cli = optional_field(&fields, "agent_cli")
            .or_else(|| legacy_unbridged_agent(&session_name).map(str::to_owned))
            .ok_or_else(|| format!("Agent Session {session_name} has no agent_cli"))?;
        let available = match fields.get("pane_dead").copied().unwrap_or("0") {
            "0" => true,
            "1" => false,
            value => {
                return Err(format!(
                    "Agent Session {session_name} has invalid pane_dead={value}"
                ));
            }
        };
        let status = SessionStatus::from_name(fields.get("status").copied().unwrap_or("idle"))?;
        let placement = PanePlacement::from_name(fields.get("placement").copied().unwrap_or(""))?;
        let self_test = match fields.get("self_test").copied().unwrap_or("0") {
            "0" => false,
            "1" => true,
            value => {
                return Err(format!(
                    "Agent Session {session_name} has invalid self_test={value}"
                ));
            }
        };
        Ok(Some(Self {
            session_name,
            agent_cli,
            agent_args: Vec::new(),
            host: field(&fields, "host"),
            project: field(&fields, "project"),
            cwd: field(&fields, "cwd"),
            control_channel: field(&fields, "control_channel"),
            operator: field(&fields, "operator"),
            status,
            placement,
            thread_ts: optional_field(&fields, "thread_ts"),
            thread_permalink: optional_field(&fields, "thread_permalink"),
            pane_id: Some(pane_id),
            tmux_session: optional_field(&fields, "tmux_session"),
            tmux_window: optional_field(&fields, "tmux_window"),
            notify_socket: optional_field(&fields, "notify_socket"),
            notify_token: optional_field(&fields, "notify_token"),
            status_file: optional_field(&fields, "status_file"),
            last_slack_prompt: optional_field(&fields, "last_slack_prompt"),
            available,
            self_test,
        }))
    }

    pub fn list_from_registry() -> Result<Vec<Self>, String> {
        let output = Command::new("tmux")
            .args(["list-panes", "-a", "-F", TMUX_REGISTRY_FORMAT])
            .output()
            .map_err(|error| format!("failed to list tmux panes: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running") {
                return Ok(Vec::new());
            }
            return Err(stderr.trim().to_owned());
        }
        let mut sessions = Vec::new();
        for (index, record) in String::from_utf8_lossy(&output.stdout).lines().enumerate() {
            if let Some(session) = Self::from_registry_record(record).map_err(|error| {
                format!("invalid tmux Agent Session record {}: {error}", index + 1)
            })? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    pub fn write_to_registry(&self) -> Result<(), String> {
        let pane_id = self.registry_pane()?;
        for (key, value) in self.registry_metadata() {
            if key == "@pingme_session_name" {
                continue;
            }
            set_registry_field(pane_id, key, &value)?;
        }
        set_registry_field(pane_id, "@pingme_session_name", &self.session_name)
    }

    pub fn name_exists(name: &str) -> Result<bool, String> {
        let output = Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{@pingme_session_name}"])
            .output()
            .map_err(|error| format!("failed to list pingme tmux sessions: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running") {
                return Ok(false);
            }
            return Err(stderr.trim().to_owned());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|session_name| session_name == name))
    }

    pub fn write_name(&mut self, name: &str) -> Result<(), String> {
        // Rename of an already-published Session; first publication goes through write_to_registry.
        set_registry_field(self.registry_pane()?, "@pingme_session_name", name)?;
        self.set_name(name);
        Ok(())
    }

    pub fn write_status(&mut self, status: SessionStatus) -> Result<(), String> {
        set_registry_field(self.registry_pane()?, "@pingme_status", status.as_str())?;
        self.set_status(status);
        Ok(())
    }

    pub fn write_self_test(&mut self, enabled: bool) -> Result<(), String> {
        set_registry_field(
            self.registry_pane()?,
            "@pingme_self_test",
            if enabled { "1" } else { "0" },
        )?;
        self.set_self_test(enabled);
        Ok(())
    }

    pub fn write_last_slack_prompt(&mut self, fingerprint: Option<String>) -> Result<(), String> {
        set_registry_field(
            self.registry_pane()?,
            "@pingme_last_slack_prompt_fingerprint",
            fingerprint.as_deref().unwrap_or(""),
        )?;
        self.set_last_slack_prompt(fingerprint);
        Ok(())
    }

    pub fn clear_registry_name(pane_id: &str) -> Result<(), String> {
        let output = Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-u",
                "-t",
                pane_id,
                "@pingme_session_name",
            ])
            .output()
            .map_err(|error| format!("failed to run tmux: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    pub fn clear_registry_name_command(pane_id: &str) -> String {
        format!("tmux set-option -p -u -t {pane_id} @pingme_session_name")
    }

    fn registry_pane(&self) -> Result<&str, String> {
        self.pane_id
            .as_deref()
            .ok_or_else(|| format!("Agent Session {} has no tmux pane", self.session_name))
    }

    pub fn enable_self_test(&mut self, status_file: String) {
        self.self_test = true;
        self.status_file = Some(status_file);
    }

    pub fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
    }

    pub fn mark_pane_dead(&mut self) {
        self.available = false;
    }

    pub fn mark_unavailable(&mut self) {
        self.status = SessionStatus::Unavailable;
        self.available = false;
    }

    pub fn set_name(&mut self, name: impl Into<String>) {
        self.session_name = name.into();
    }

    pub fn set_last_slack_prompt(&mut self, fingerprint: Option<String>) {
        self.last_slack_prompt = fingerprint;
    }

    pub fn set_self_test(&mut self, enabled: bool) {
        self.self_test = enabled;
    }

    pub fn fill_missing_from_settings(&mut self, settings: &HostSettings) {
        if self.host.is_empty() {
            self.host.clone_from(&settings.host.name);
        }
        if self.cwd.is_empty()
            && let Some(project) = settings
                .projects
                .iter()
                .find(|project| project.name == self.project)
        {
            self.cwd.clone_from(&project.cwd);
        }
        if self.cwd.is_empty() {
            self.cwd = "unknown".to_owned();
        }
        if self.control_channel.is_empty() {
            self.control_channel
                .clone_from(&settings.slack.control_channel_id);
        }
        if self.operator.is_empty() {
            self.operator.clone_from(&settings.slack.operator_id);
        }
    }

    fn registry_metadata(&self) -> Vec<(&'static str, String)> {
        // Session Name is last so a crash mid-write leaves the pane unpublished.
        vec![
            ("@pingme_agent_cli", self.agent_cli.clone()),
            ("@pingme_host", self.host.clone()),
            ("@pingme_project", self.project.clone()),
            ("@pingme_cwd", self.cwd.clone()),
            ("@pingme_status", self.status.as_str().to_owned()),
            (
                "@pingme_thread_ts",
                self.thread_ts.clone().unwrap_or_default(),
            ),
            (
                "@pingme_thread_permalink",
                self.thread_permalink.clone().unwrap_or_default(),
            ),
            ("@pingme_channel", self.control_channel.clone()),
            ("@pingme_operator", self.operator.clone()),
            (
                "@pingme_self_test",
                if self.self_test { "1" } else { "0" }.to_owned(),
            ),
            (
                "@pingme_placement",
                self.placement
                    .map(PanePlacement::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            (
                "@pingme_last_slack_prompt_fingerprint",
                self.last_slack_prompt.clone().unwrap_or_default(),
            ),
            (
                "@pingme_notify_socket",
                self.notify_socket.clone().unwrap_or_default(),
            ),
            (
                "@pingme_notify_token",
                self.notify_token.clone().unwrap_or_default(),
            ),
            (
                "@pingme_status_file",
                self.status_file.clone().unwrap_or_default(),
            ),
            ("@pingme_session_name", self.session_name.clone()),
        ]
    }
}

impl PanePlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReuseCurrentPane => "reuse_current_pane",
            Self::DetachedWindow => "detached_window",
            Self::DetachedSession { attach: true } => "detached_session_attach",
            Self::DetachedSession { attach: false } => "detached_session",
        }
    }

    fn from_name(name: &str) -> Result<Option<Self>, String> {
        match name {
            "" => Ok(None),
            "reuse_current_pane" => Ok(Some(Self::ReuseCurrentPane)),
            "detached_window" => Ok(Some(Self::DetachedWindow)),
            "detached_session_attach" => Ok(Some(Self::DetachedSession { attach: true })),
            "detached_session" => Ok(Some(Self::DetachedSession { attach: false })),
            _ => Err(format!("invalid Agent Session placement={name}")),
        }
    }
}

impl SessionStatus {
    fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "starting" => Ok(Self::Starting),
            "" | "idle" => Ok(Self::Idle),
            "busy" => Ok(Self::Busy),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(format!("invalid Agent Session status={name}")),
        }
    }
}

fn field(fields: &BTreeMap<&str, &str>, name: &str) -> String {
    fields.get(name).copied().unwrap_or_default().to_owned()
}

fn optional_field(fields: &BTreeMap<&str, &str>, name: &str) -> Option<String> {
    fields
        .get(name)
        .copied()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn legacy_unbridged_agent(session_name: &str) -> Option<&'static str> {
    ["codex", "grok"].into_iter().find(|agent_cli| {
        let base = format!("{agent_cli}-unbridged");
        session_name == base
            || session_name
                .strip_prefix(&format!("{base}-"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
                })
    })
}

fn set_registry_field(pane_id: &str, key: &str, value: &str) -> Result<(), String> {
    let output = Command::new("tmux")
        .args(["set-option", "-p", "-t", pane_id, key, value])
        .output()
        .map_err(|error| format!("failed to run tmux: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentSession, PanePlacement, SessionStatus};

    #[test]
    fn registry_constructor_recovers_agent_session_facts() {
        let session = AgentSession::from_registry_record(
            "pane_id=%7\ttmux_session=bridge\ttmux_window=bridge:3\tpane_dead=0\tsession_name=codex-project\tagent_cli=codex\thost=linux\tproject=project\tcwd=/work/project\tstatus=busy\tthread_ts=100.1\tcontrol_channel=C1\toperator=U1\tnotify_socket=/tmp/bridge.sock\tnotify_token=secret\tself_test=1\tstatus_file=/tmp/status\tlast_slack_prompt=abc123\tplacement=detached_window",
        )
        .unwrap()
        .unwrap();

        assert_eq!("codex-project", session.session_name);
        assert_eq!("codex", session.agent_cli);
        assert_eq!("linux", session.host);
        assert_eq!("project", session.project);
        assert_eq!("/work/project", session.cwd);
        assert_eq!(Some("%7"), session.pane_id.as_deref());
        assert_eq!(Some("bridge"), session.tmux_session.as_deref());
        assert_eq!(Some("bridge:3"), session.tmux_window.as_deref());
        assert_eq!(Some("100.1"), session.thread_ts.as_deref());
        assert_eq!("C1", session.control_channel);
        assert_eq!("U1", session.operator);
        assert_eq!(Some("/tmp/bridge.sock"), session.notify_socket.as_deref());
        assert_eq!(Some("secret"), session.notify_token.as_deref());
        assert_eq!(Some("/tmp/status"), session.status_file.as_deref());
        assert_eq!(Some("abc123"), session.last_slack_prompt.as_deref());
        assert_eq!(SessionStatus::Busy, session.status);
        assert_eq!(Some(PanePlacement::DetachedWindow), session.placement);
        assert!(session.available);
        assert!(session.self_test);
    }

    #[test]
    fn registry_constructor_tolerates_optional_and_unknown_fields() {
        let session = AgentSession::from_registry_record(
            "pane_id=%7\tpane_dead=1\tsession_name=codex-project\tagent_cli=codex\tstatus=idle\tunknown_future_field=value",
        )
        .unwrap()
        .unwrap();

        assert_eq!("codex-project", session.session_name);
        assert_eq!("codex", session.agent_cli);
        assert_eq!(None, session.notify_token);
        assert_eq!(None, session.placement);
        assert_eq!(SessionStatus::Idle, session.status);
        assert!(!session.available);
        assert_ne!(SessionStatus::Unavailable, session.status);
    }

    #[test]
    fn registry_constructor_ignores_non_agent_panes() {
        assert!(
            AgentSession::from_registry_record("pane_id=%7\tpane_dead=0\tsession_name=")
                .unwrap()
                .is_none()
        );
        assert!(
            AgentSession::from_registry_record("session_name=broken\tstatus=wat")
                .unwrap_err()
                .contains("no pane_id")
        );
        assert!(
            AgentSession::from_registry_record(
                "pane_id=%7\tsession_name=broken\tagent_cli=codex\tstatus=wat",
            )
            .unwrap_err()
            .contains("invalid Agent Session status")
        );
        assert!(
            AgentSession::from_registry_record("pane_id=%7\tsession_name=broken")
                .unwrap_err()
                .contains("no agent_cli")
        );
        let legacy = AgentSession::from_registry_record(
            "pane_id=%7\tpane_dead=0\tsession_name=codex-unbridged-2",
        )
        .unwrap()
        .unwrap();
        assert_eq!("codex", legacy.agent_cli);
        assert_eq!(None, legacy.thread_ts);
    }

    #[test]
    fn registry_write_publishes_session_name_last() {
        let mut session = AgentSession::for_launch(
            "codex-project",
            "codex",
            "linux",
            "project",
            "/work/project",
            "C1",
            "U1",
            PanePlacement::DetachedWindow,
        );
        session.thread_ts = Some("100.1".to_owned());
        session.pane_id = Some("%7".to_owned());
        session.tmux_window = Some("bridge:3".to_owned());

        let keys: Vec<_> = session
            .registry_metadata()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(Some("@pingme_session_name"), keys.last().copied());
        assert_eq!(
            1,
            keys.iter()
                .filter(|name| **name == "@pingme_session_name")
                .count()
        );
    }
}
