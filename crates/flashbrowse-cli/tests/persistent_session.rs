//! Regression test for GitHub issue #1:
//! `open --session-id <id>` must keep Chromium alive after the CLI exits

use std::process::Command;

struct SessionGuard {
    bin: &'static str,
    id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(self.bin)
            .args(["close", "--session-id", &self.id])
            .output();
    }
}

#[test]
fn persistent_session_survives_cli_exit_and_accepts_act() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("skipping persistent_session test: no Chrome binary found");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_flashbrowse");
    let session_id = format!("test-persist-{}", std::process::id());
    let _guard = SessionGuard {
        bin,
        id: session_id.clone(),
    };

    let url = "data:text/html,<button onclick=\"document.body.append('clicked')\">Go</button>";
    let open = Command::new(bin)
        .args(["open", url, "--session-id", &session_id])
        .output()
        .expect("run flashbrowse open");
    assert!(
        open.status.success(),
        "open failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&open.stdout),
        String::from_utf8_lossy(&open.stderr)
    );
    let open_out = String::from_utf8_lossy(&open.stdout);
    assert!(open_out.contains("[1] Button: \"Go\""), "{open_out}");

    // The `open` CLI process has now exited; `act` must still reach Chromium.
    let act = Command::new(bin)
        .args(["act", "1", "--session-id", &session_id, "--action", "click"])
        .output()
        .expect("run flashbrowse act");
    assert!(
        act.status.success(),
        "act failed after open exited: stdout={}, stderr={}",
        String::from_utf8_lossy(&act.stdout),
        String::from_utf8_lossy(&act.stderr)
    );
    let act_out = String::from_utf8_lossy(&act.stdout);
    assert!(act_out.contains("clicked"), "{act_out}");

    let close = Command::new(bin)
        .args(["close", "--session-id", &session_id])
        .output()
        .expect("run flashbrowse close");
    assert!(
        close.status.success(),
        "close failed: stderr={}",
        String::from_utf8_lossy(&close.stderr)
    );
}

#[test]
fn closing_persistent_session_preserves_caller_profile_in_temp() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        return;
    }
    let bin = env!("CARGO_BIN_EXE_flashbrowse");
    let id = format!("test-profile-{}", std::process::id());
    let root =
        std::env::temp_dir().join(format!("flashbrowse-profile-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("caller-data");
    std::fs::write(&marker, b"preserve").unwrap();
    let _guard = SessionGuard {
        bin,
        id: id.clone(),
    };
    let open = Command::new(bin)
        .args([
            "open",
            "data:text/html,<h1>Profile</h1>",
            "--session-id",
            &id,
            "--user-data-dir",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let close = Command::new(bin)
        .args(["close", "--session-id", &id])
        .output()
        .unwrap();
    assert!(
        close.status.success(),
        "{}",
        String::from_utf8_lossy(&close.stderr)
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"preserve");
    remove_dir_all_retrying(&root);
}

/// `close` returns as soon as the browser tree is signalled, but Chromium
/// children can still be releasing profile files for a moment afterwards.
/// Windows reports a sharing violation (code 32) while that happens, so retry
/// briefly. Cleanup of the test's temp dir must never fail the assertion.
fn remove_dir_all_retrying(path: &std::path::Path) {
    let started = std::time::Instant::now();
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if started.elapsed() < std::time::Duration::from_secs(5) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}
