//! `flashbrowse close --session-id <id>` - stop a persistent browser.

use super::session_file::{
    kill_pid, pid_alive, pid_belongs_to_chrome, read_session, remove_session,
};
use flashbrowse_core::observability;

/// Kill the detached Chromium and drop the session file.
/// Refuses to kill if the pid no longer belongs to Chrome (H2).
pub async fn run(session_id: String) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let persisted =
        read_session(&session_id)?.ok_or_else(|| anyhow::anyhow!("no session '{session_id}'"))?;
    let mut message = None;
    if persisted.pid != 0 && pid_alive(persisted.pid) {
        if pid_belongs_to_chrome(persisted.pid) {
            kill_pid(persisted.pid)?;
            message = Some(format!("killed browser pid {}", persisted.pid));
        } else {
            message = Some(format!(
                "pid {} no longer belongs to Chrome - refusing to kill (possible PID reuse)",
                persisted.pid
            ));
        }
    } else if persisted.pid != 0 {
        message = Some(format!(
            "session browser pid {} already gone",
            persisted.pid
        ));
    }
    remove_session(&session_id)?;
    if let Some(message) = message {
        crate::write_stdout(format_args!("{message}"))?;
    }
    crate::write_stdout(format_args!("removed session '{session_id}'"))?;
    observability::record_op("close", &session_id, started, None, true, None);
    Ok(())
}
