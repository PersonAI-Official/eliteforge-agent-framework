#![cfg(unix)]
use eliteforge_agent_official_plugin_tools_local::capture::*;
use std::time::Duration;
fn spec(dir: &tempfile::TempDir, script: &str) -> CapturedCommand {
    CapturedCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: dir.path().into(),
        env: [("CAPTURE_VALUE".into(), "literal; $(touch injected)".into())].into(),
        env_remove: vec!["HOME".into()],
        timeout: Duration::from_secs(2),
        max_output_bytes: 4096,
    }
}
#[tokio::test]
async fn capture_preserves_streams_exit_status_and_environment_policy() {
    let dir = tempfile::tempdir().unwrap();
    let result = capture_command(spec(
        &dir,
        "printf '%s' \"$CAPTURE_VALUE\"; printf '%s' \"${HOME-unset}\" >&2; exit 7",
    ))
    .await
    .unwrap();
    assert_eq!(result["stdout"]["content"], "literal; $(touch injected)");
    assert_eq!(result["stderr"]["content"], "unset");
    assert_eq!(result["exitCode"], 7);
    assert!(!dir.path().join("injected").exists());
}
#[test]
fn blocking_capture_enforces_timeout_and_cleans_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = spec(&dir, "sleep 60 & echo $! > child.pid; wait");
    command.timeout = Duration::from_millis(100);
    let result = capture_command_blocking(command).unwrap();
    assert_eq!(result["status"], "timed_out");
    let pid = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
    let status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.trim().is_empty() || status.trim().starts_with('Z'),
        "{status}"
    );
}
