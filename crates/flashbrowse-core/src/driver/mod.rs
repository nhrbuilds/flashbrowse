//! Browser engine seam (Chrome CDP today; Firefox BiDi later).
//!
//! [`CdpDriver`] is the production implementation. A future `BiDiDriver` will
//! live in the `flashbrowse-bidi` crate and plug in behind the same surface.
//!
//! The seam covers the **engine-neutral** primitives: navigation state,
//! in-page evaluation, the accessibility tree and screen capture. The input
//! and DOM-query primitives (`click`, `fill`, `select`) are still CDP-specific
//! and go straight to the CDP client - a BiDi driver would have to reimplement
//! them before a Firefox session can act on a page.

mod cdp;

use std::collections::HashMap;

use async_trait::async_trait;
use flashbrowse_cdp::protocol::accessibility::AXNode;
use serde_json::Value;

use crate::resource_policy::ResourcePolicy;
use crate::CoreError;

pub use cdp::CdpDriver;

/// CDP session and parent `<iframe>` owner handle for an out-of-process iframe.
#[derive(Debug, Clone)]
pub struct FrameSession {
    pub session_id: String,
    pub owner_backend: Option<i64>,
}

/// Engine-neutral browser primitives (one flattened page target).
#[async_trait]
pub trait BrowserDriver: Send + Sync {
    /// Apply the session's network resource policy.
    async fn set_resource_policy(&self, policy: ResourcePolicy) -> Result<(), CoreError>;
    /// Replace the network blocklist with `patterns`.
    async fn set_blocklist(&self, patterns: Vec<String>) -> Result<(), CoreError>;
    /// Evaluate an expression in the page and return its value.
    async fn evaluate(&self, expression: &str) -> Result<Value, CoreError>;
    /// Fetch the full accessibility tree.
    async fn get_full_ax_tree(&self) -> Result<Vec<AXNode>, CoreError>;
    /// Fetch the full accessibility tree plus `node_id -> FrameSession`
    /// overrides for out-of-process iframes (OOPIFs).
    async fn get_full_ax_tree_with_sessions(
        &self,
    ) -> Result<(Vec<AXNode>, HashMap<String, FrameSession>), CoreError> {
        Ok((self.get_full_ax_tree().await?, HashMap::new()))
    }
    /// Capture a PNG screenshot (base64).
    async fn capture_screenshot(&self, full_page: bool) -> Result<String, CoreError>;
    /// Capture a PNG screenshot (base64) with an optional clip viewport.
    async fn capture_screenshot_clipped(
        &self,
        full_page: bool,
        clip: Option<flashbrowse_cdp::protocol::page::Viewport>,
    ) -> Result<String, CoreError> {
        let _ = clip;
        self.capture_screenshot(full_page).await
    }
}

#[async_trait]
impl BrowserDriver for CdpDriver {
    async fn set_resource_policy(&self, policy: ResourcePolicy) -> Result<(), CoreError> {
        CdpDriver::set_resource_policy(self, policy).await
    }

    async fn set_blocklist(&self, patterns: Vec<String>) -> Result<(), CoreError> {
        CdpDriver::set_blocklist(self, patterns).await
    }

    async fn evaluate(&self, expression: &str) -> Result<Value, CoreError> {
        CdpDriver::evaluate(self, expression).await
    }

    async fn get_full_ax_tree(&self) -> Result<Vec<AXNode>, CoreError> {
        CdpDriver::get_full_ax_tree(self).await
    }

    async fn get_full_ax_tree_with_sessions(
        &self,
    ) -> Result<(Vec<AXNode>, HashMap<String, FrameSession>), CoreError> {
        CdpDriver::get_full_ax_tree_with_sessions(self).await
    }

    async fn capture_screenshot(&self, full_page: bool) -> Result<String, CoreError> {
        CdpDriver::capture_screenshot(self, full_page).await
    }

    async fn capture_screenshot_clipped(
        &self,
        full_page: bool,
        clip: Option<flashbrowse_cdp::protocol::page::Viewport>,
    ) -> Result<String, CoreError> {
        CdpDriver::capture_screenshot_clipped(self, full_page, clip).await
    }
}
