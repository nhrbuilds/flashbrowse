use super::{parse_devtools_ws_url, ChromeProcess};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PROFILE_ID: AtomicU64 = AtomicU64::new(1);

#[test]
fn parses_devtools_ws_url() {
    assert_eq!(
        parse_devtools_ws_url(
            "\nDevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123-DEF\r\n"
        ),
        Some("ws://127.0.0.1:9222/devtools/browser/abc-123-DEF")
    );
    assert_eq!(
        parse_devtools_ws_url("DevTools listening on ws://[::1]:9222/devtools/browser/0123-abcd"),
        Some("ws://[::1]:9222/devtools/browser/0123-abcd")
    );
    assert_eq!(
        parse_devtools_ws_url(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/dead-beef, extra"
        ),
        Some("ws://127.0.0.1:9222/devtools/browser/dead-beef")
    );
    assert_eq!(
        parse_devtools_ws_url("DevTools listening on http://127.0.0.1:9222/devtools/browser/abc"),
        None
    );
    assert_eq!(
        parse_devtools_ws_url("DevTools listening on ws://127.0.0.1:9222/devtools/browser/"),
        None
    );
}

#[test]
fn owned_profile_is_removed_when_process_guard_drops_during_unwind() {
    let id = NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
    let user_data_dir = std::env::temp_dir().join(format!(
        "flashbrowse-panic-drop-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir(&user_data_dir).expect("create temporary profile");

    let result = std::panic::catch_unwind(|| {
        let process = ChromeProcess {
            child: None,
            browser_ws_url: String::new(),
            chrome_path: PathBuf::new(),
            user_data_dir: user_data_dir.clone(),
            owned_dir: true,
            #[cfg(unix)]
            pipe_handles: None,
        };
        let _process = process;
        panic!("simulate an operation panic");
    });

    assert!(result.is_err());
    assert!(!user_data_dir.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn owned_process_group_is_killed_when_guard_drops_during_unwind() {
    use super::{configure_process_group, kill_chrome_process_group_without_panicking};
    use tokio::process::Command;
    use tokio::time::{sleep, timeout, Duration};

    let id = NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
    let test_dir = std::env::temp_dir().join(format!(
        "flashbrowse-process-drop-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir(&test_dir).expect("create process test directory");
    let marker = test_dir.join("descendant-writes");
    let user_data_dir = test_dir.join("profile");
    std::fs::create_dir(&user_data_dir).expect("create temporary profile");

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sh -c 'while :; do printf x >> \"$1\"; sleep 0.01; done' sh \"$1\" & wait")
        .arg("flashbrowse-test")
        .arg(&marker)
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let child = command.spawn().expect("start process group test child");
    let pid = child.id().expect("test child process id");

    timeout(Duration::from_secs(2), async {
        while !marker.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant starts writing before panic");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let process = ChromeProcess {
            child: Some(child),
            browser_ws_url: String::new(),
            chrome_path: PathBuf::new(),
            user_data_dir: user_data_dir.clone(),
            owned_dir: true,
            pipe_handles: None,
        };
        let _process = process;
        panic!("simulate an operation panic");
    }));
    assert!(result.is_err());

    sleep(Duration::from_millis(50)).await;
    let stopped_at = std::fs::metadata(&marker)
        .expect("read descendant marker")
        .len();
    sleep(Duration::from_millis(50)).await;
    let stopped = std::fs::metadata(&marker)
        .expect("read descendant marker after wait")
        .len()
        == stopped_at;
    if !stopped {
        kill_chrome_process_group_without_panicking(pid);
    }
    assert!(
        stopped,
        "Chrome descendant kept running after unwind cleanup"
    );
    assert!(!user_data_dir.exists());
    std::fs::remove_dir_all(test_dir).expect("remove process test directory");
}
