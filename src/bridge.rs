use crate::{
    AgentSession, LocalServices, SELF_TEST_PROMPT, SessionStatus, ThreadAction, ThreadMessage,
    self_test_message, thread_action,
};

pub struct Bridge {
    control_channel_id: String,
    operator_id: String,
}

pub struct RoutedThreadAction<'a> {
    pub session: &'a AgentSession,
    pub action: ThreadAction,
}

pub struct StartedSession {
    pub session: AgentSession,
    pub status_warning: Option<String>,
    pub registration_error: Option<String>,
}

impl Bridge {
    pub fn new(control_channel_id: impl Into<String>, operator_id: impl Into<String>) -> Self {
        Self {
            control_channel_id: control_channel_id.into(),
            operator_id: operator_id.into(),
        }
    }

    pub fn start_agent_session(
        &self,
        session: AgentSession,
        services: &mut dyn LocalServices,
    ) -> Result<StartedSession, String> {
        let mut session = services.spawn_agent(session)?;
        session.set_status(SessionStatus::Idle);
        let status_warning = services
            .update_session_thread(&session)
            .err()
            .map(|error| format!("failed to update Slack Session Thread: {error}\n"));
        let registration_error = services.register_session(&session).err().map(|error| {
            format!(
                "failed to start Slack daemon: {error}\nSlack control is unavailable for this session\n"
            )
        });
        Ok(StartedSession {
            session,
            status_warning,
            registration_error,
        })
    }

    pub fn route_thread_message<'session>(
        &self,
        message: &ThreadMessage<'_>,
        sessions: &'session [AgentSession],
    ) -> Option<RoutedThreadAction<'session>> {
        let thread_ts = message.thread_ts?;
        let session = sessions.iter().find(|session| {
            session.thread_ts.as_deref() == Some(thread_ts)
                && session.control_channel == self.control_channel_id
        })?;
        let action = if session.self_test
            && message.is_bot
            && session
                .notify_token
                .as_deref()
                .is_some_and(|token| message.text == self_test_message(token))
        {
            ThreadAction::Prompt(SELF_TEST_PROMPT.to_owned())
        } else {
            thread_action(
                message,
                &self.control_channel_id,
                thread_ts,
                &self.operator_id,
            )
        };
        Some(RoutedThreadAction { session, action })
    }

    pub fn route_notification<'session>(
        &self,
        token: &str,
        sessions: &'session [AgentSession],
    ) -> Option<&'session AgentSession> {
        sessions.iter().find(|session| {
            session.control_channel == self.control_channel_id
                && session.notify_token.as_deref() == Some(token)
        })
    }
}
