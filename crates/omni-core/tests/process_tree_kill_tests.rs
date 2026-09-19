//! Regression test for defect D-03: a timed-out tool must take its children
//! with it.
//!
//! yt-dlp spawns ffmpeg to mux fragments; ffmpeg spawns nothing but holds the
//! output file open. Killing only the direct child leaves the grandchild
//! running, holding the per-job temp directory, so cleanup fails and the retry
//! collides with a half-written file. The Job Object in `omni_core::process`
//! exists to prevent exactly that.

#![cfg(windows)]

use std::path::Path;
use std::time::Duration;

use omni_core::process::{run, RunOpts};

/// Count `ping.exe` processes carrying a marker in their command line.
///
/// `tasklist` cannot filter on command line, so this uses CIM via PowerShell —
/// the same query an operator would run to check for orphans.
fn count_marked_pings(marker: &str) -> usize {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "@(Get-CimInstance Win32_Process -Filter \"Name='ping.exe'\" | \
                  Where-Object {{ $_.CommandLine -like '*{marker}*' }}).Count"
            ),
        ])
        .output()
        .expect("powershell must be available on Windows");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[tokio::test]
async fn timeout_kills_the_whole_process_tree() {
    // A unique marker so a developer's unrelated pings never match.
    let marker = format!("omnitreetest{}", std::process::id());

    // cmd.exe (the child) starts a detached ping (the grandchild) that runs for
    // 10 minutes — far longer than any plausible test window, so if the tree is
    // not killed the assertion below cannot pass by the grandchild simply
    // finishing on its own. The marker rides in the grandchild's command line.
    let script = format!(
        "start /b ping -n 600 127.0.0.1 -l 32 -w 1000 ::{marker}:: > nul & \
         ping -n 600 127.0.0.1 > nul"
    );

    let started = std::time::Instant::now();
    let out = run(
        Path::new("cmd"),
        ["/c", &script],
        RunOpts::new(Duration::from_millis(1500)),
    )
    .await
    .expect("spawn should succeed");

    assert!(out.timed_out, "expected the run to time out, got {out:?}");
    // The runner must return near the timeout, not wait for the tree's pipes to
    // close. Without the bounded drain this took the grandchild's full runtime.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "run() blocked for {:?} after a 1.5 s timeout",
        started.elapsed()
    );

    // The kernel tears the job down asynchronously; allow a short settle.
    let mut remaining = usize::MAX;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        remaining = count_marked_pings(&marker);
        if remaining == 0 {
            break;
        }
    }

    assert_eq!(
        remaining, 0,
        "grandchild ping processes survived the timeout; the job object did not \
         kill the tree (defect D-03)"
    );
}
