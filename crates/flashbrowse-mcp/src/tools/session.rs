use super::SessionManager;
use flashbrowse_core::BrowserSession;

use crate::protocol::McpError;
use std::collections::HashMap;

impl SessionManager {
    pub fn new() -> Self {
        Self {
            active: "default".to_string(),
            sessions: HashMap::new(),
        }
    }

    pub fn active_name(&self) -> &str {
        &self.active
    }

    pub fn as_ref(&self) -> Option<&BrowserSession> {
        self.sessions.get(&self.active)
    }

    pub fn as_mut(&mut self) -> Option<&mut BrowserSession> {
        self.sessions.get_mut(&self.active)
    }

    pub fn is_none(&self) -> bool {
        self.as_ref().is_none()
    }

    pub fn session_mut_for(
        &mut self,
        context: Option<&str>,
    ) -> Result<&mut BrowserSession, McpError> {
        let name = context.unwrap_or(&self.active).to_string();
        self.sessions.get_mut(&name).ok_or_else(|| {
            if name == "default" {
                McpError::Internal("no browser session: call browser_open first".to_string())
            } else {
                McpError::InvalidParams(format!("unknown browser context '{name}'"))
            }
        })
    }

    pub async fn shutdown_all(&mut self) {
        if let Err(error) = flashbrowse_core::async_runtime::run(self.shutdown_all_inner()).await {
            tracing::warn!(target: "flashbrowse_mcp::tools", %error, "browser session cleanup could not start");
        }
    }

    async fn shutdown_all_inner(&mut self) {
        // Remove every session from shared state before awaiting teardown. If
        // this future is cancelled or a session teardown fails, the manager is
        // already clean and dropping remaining sessions still triggers their
        // browser process guards.
        let mut sessions = std::mem::take(&mut self.sessions);
        self.active = "default".to_string();
        let default_session = sessions.remove("default");
        for (name, session) in sessions {
            shutdown_session(name, session).await;
        }
        if let Some(session) = default_session {
            shutdown_session("default".to_string(), session).await;
        }
    }
}

async fn shutdown_session(name: String, session: BrowserSession) {
    match flashbrowse_core::async_runtime::run(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.shutdown(),
    ))
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::warn!(target: "flashbrowse_mcp::tools", context = %name, "browser session cleanup timed out; session dropped")
        }
        Err(error) => {
            tracing::error!(target: "flashbrowse_mcp::tools", context = %name, %error, "browser session cleanup panicked and was contained");
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
