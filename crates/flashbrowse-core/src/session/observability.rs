use super::*;

impl BrowserSession {
    /// Captured console messages and uncaught page exceptions (bounded to 200 entries).
    pub fn console_entries(&self, level: Option<&str>) -> Vec<ConsoleEntry> {
        self.console.list(level)
    }

    /// Clear captured console entries.
    pub fn clear_console(&self) {
        self.console.clear();
    }

    /// Captured network request/response entries (bounded to 200 entries).
    pub fn network_entries(&self, failed_only: bool) -> Vec<NetworkEntry> {
        self.network_log.list(failed_only)
    }

    /// Fetch the response body for a captured `request_id` (capped at 64 KiB text).
    pub async fn network_response_body(&self, request_id: &str) -> Result<String, CoreError> {
        let resp = network::get_response_body(&self.client, &self.session_id, request_id).await?;
        let text = if resp.base64_encoded {
            match flashbrowse_cdp::protocol::fetch::decode_base64(&resp.body) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => resp.body,
            }
        } else {
            resp.body
        };
        const MAX_BODY_BYTES: usize = 64 * 1024;
        if text.len() > MAX_BODY_BYTES {
            let end = text.floor_char_boundary(MAX_BODY_BYTES);
            Ok(format!(
                "{}...(truncated)",
                text.get(..end).unwrap_or_default()
            ))
        } else {
            Ok(text)
        }
    }

    /// Clear captured network log entries.
    pub fn clear_network(&self) {
        self.network_log.clear();
    }

    /// Set extra HTTP headers sent on every subsequent request in this session.
    pub async fn set_extra_headers(
        &mut self,
        headers: HashMap<String, String>,
    ) -> Result<(), CoreError> {
        network::set_extra_http_headers(&self.client, &self.session_id, &headers).await?;
        self.extra_headers = headers;
        Ok(())
    }

    /// Add a lazy `Fetch` route mock or abort rule.
    pub async fn add_route(&self, rule: RouteRule) -> Result<(), CoreError> {
        self.routes
            .add_rule(&self.client, &self.session_id, rule)
            .await?;
        Ok(())
    }

    /// List active `Fetch` route rules.
    pub fn list_routes(&self) -> Vec<RouteRule> {
        self.routes.list_rules()
    }

    /// Clear all active `Fetch` route rules and disable `Fetch` interception.
    pub async fn clear_routes(&self) -> Result<(), CoreError> {
        self.routes
            .clear_rules(&self.client, &self.session_id)
            .await?;
        Ok(())
    }
}
