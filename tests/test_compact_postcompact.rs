//! Integration tests for `squeez compact-summary` (the PostCompact hook
//! entrypoint), spawning the real binary (same pattern as
//! tests/test_flag_force.rs). Covers the fix for the invalid
//! `hookSpecificOutput.hookEventName:"PostCompact"` schema: PostCompact is
//! display-only per the Claude Code hooks reference and doesn't support
//! `additionalContext`, so the hook must write a pending file for the
//! UserPromptSubmit relay to pick up instead of printing invalid hook JSON.

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> String {
    env!("CARGO_BIN_EXE_squeez").to_string()
}

fn tmp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeez_postcompact_it_{}_{}",
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("squeez").join("sessions")).unwrap();
    std::fs::create_dir_all(dir.join("squeez").join("memory")).unwrap();
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn run_compact_summary(squeez_dir: &std::path::Path, state_dir: &std::path::Path, stdin: &str) -> std::process::Output {
    let mut child = Command::new(bin())
        .arg("compact-summary")
        .env("SQUEEZ_DIR", squeez_dir.join("squeez"))
        .env("SQUEEZ_STATE_DIR", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Seed a recognizable file under seen_files via the real `track-result`
/// PostToolUse path (not a hand-written context.json) so build_summary() has
/// something to say. context.json uses a hand-rolled flat parallel-array
/// format internal to SessionContext -- going through the real command is
/// the only way to populate it without depending on that private shape.
fn seed_session_state(squeez_dir: &std::path::Path) {
    let mut child = Command::new(bin())
        .args(["track-result", "Read"])
        .env("SQUEEZ_DIR", squeez_dir.join("squeez"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"Read","file_path":"src/main.rs"}"#)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn postcompact_never_prints_invalid_hook_json() {
    let dir = tmp_dir("no_invalid_json");
    seed_session_state(&dir);
    let out = run_compact_summary(
        &dir,
        &dir.join("state"),
        r#"{"session_id":"abc","hook_event_name":"PostCompact"}"#,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The bug this regresses: squeez used to print
    // {"hookSpecificOutput":{"hookEventName":"PostCompact",...}} on stdout,
    // which the host rejects as invalid for the PostCompact event.
    assert!(
        !stdout.contains("hookEventName"),
        "PostCompact hook must never print hookSpecificOutput on stdout, got: {stdout}"
    );
    assert!(out.status.success());
}

#[test]
fn postcompact_writes_pending_file_keyed_by_hashed_session_id() {
    let dir = tmp_dir("pending_file");
    seed_session_state(&dir);
    let state_dir = dir.join("state");
    run_compact_summary(&dir, &state_dir, r#"{"session_id":"abc"}"#);

    // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    // first 32 hex chars, matching dcoder's userprompt_guard.sh derivation.
    let expected = state_dir.join("postcompact.pending.ba7816bf8f01cfea414140de5dae2223.json");
    assert!(
        expected.is_file(),
        "expected pending file at {}, dir contains: {:?}",
        expected.display(),
        std::fs::read_dir(&state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    let body = std::fs::read_to_string(&expected).unwrap();
    assert!(body.contains("\"additionalContext\""));
    assert!(body.contains("src/main.rs"));
}

#[test]
fn postcompact_without_session_id_writes_nothing_and_does_not_fail() {
    let dir = tmp_dir("no_session_id");
    seed_session_state(&dir);
    let state_dir = dir.join("state");
    let out = run_compact_summary(&dir, &state_dir, "{}");
    assert!(out.status.success());
    let entries: Vec<_> = std::fs::read_dir(&state_dir).unwrap().collect();
    assert!(entries.is_empty(), "expected no pending file written without a session_id");
}

#[test]
fn postcompact_with_no_session_state_is_a_silent_noop() {
    let dir = tmp_dir("empty_state");
    // Deliberately not seeding context.json -- build_summary() has nothing
    // to report, so nothing should be written or printed.
    let state_dir = dir.join("state");
    let out = run_compact_summary(&dir, &state_dir, r#"{"session_id":"abc"}"#);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim().is_empty());
    let entries: Vec<_> = std::fs::read_dir(&state_dir).unwrap().collect();
    assert!(entries.is_empty());
}
