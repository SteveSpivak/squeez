use std::path::PathBuf;

fn tmp_dir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "squeez_sess_{}_{}",
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn test_new_session_filename_format() {
    let name = squeez::session::new_session_filename();
    // YYYY-MM-DD-HH.jsonl
    assert!(name.ends_with(".jsonl"), "got: {}", name);
    let stem = name.trim_end_matches(".jsonl");
    let parts: Vec<&str> = stem.split('-').collect();
    assert_eq!(parts.len(), 4, "got: {}", name);
    assert_eq!(parts[0].len(), 4); // year
    assert_eq!(parts[1].len(), 2); // month
    assert_eq!(parts[2].len(), 2); // day
    assert_eq!(parts[3].len(), 2); // hour
}

#[test]
fn test_unix_to_date() {
    // 2026-03-23 00:00:00 UTC = 1774224000
    let date = squeez::session::unix_to_date(1_774_224_000);
    assert_eq!(date, "2026-03-23");
}

#[test]
fn test_current_session_roundtrip() {
    let dir = tmp_dir("roundtrip");
    let s = squeez::session::CurrentSession {
        session_file: "2026-03-23-14.jsonl".to_string(),
        total_tokens: 42_000,
        tokens_saved: 0,
        total_calls: 0,
        compact_warned: true,
        state_warned: false,
        start_ts: 1_774_656_000,
        overhead_tokens: 0,
    };
    s.save(&dir);
    let loaded = squeez::session::CurrentSession::load(&dir).unwrap();
    assert_eq!(loaded.session_file, "2026-03-23-14.jsonl");
    assert_eq!(loaded.total_tokens, 42_000);
    assert!(loaded.compact_warned);
    assert_eq!(loaded.start_ts, 1_774_656_000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_current_session_missing_returns_none() {
    let dir = tmp_dir("missing");
    assert!(squeez::session::CurrentSession::load(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_append_event_creates_and_appends() {
    let dir = tmp_dir("append");
    squeez::session::append_event(&dir, "2026-03-23-14.jsonl", r#"{"type":"tool","ts":1}"#);
    squeez::session::append_event(&dir, "2026-03-23-14.jsonl", r#"{"type":"bash","ts":2}"#);
    let content = std::fs::read_to_string(dir.join("2026-03-23-14.jsonl")).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("tool"));
    assert!(lines[1].contains("bash"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_current_session_malformed_json_returns_defaults() {
    let dir = tmp_dir("malformed");
    std::fs::write(dir.join("current.json"), b"not valid json {{{{").unwrap();
    // load() is tolerant: malformed JSON must not panic.
    // Fields fall back to defaults (empty string, 0, false) rather than None.
    let result = squeez::session::CurrentSession::load(&dir);
    assert!(result.is_some(), "malformed JSON should not panic — returns default struct");
    let s = result.unwrap();
    assert_eq!(s.total_tokens, 0);
    assert!(!s.compact_warned);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_unix_to_date_leap_year() {
    // 2024-02-29 00:00:00 UTC = 1709164800
    let date = squeez::session::unix_to_date(1_709_164_800);
    assert_eq!(date, "2024-02-29", "got: {}", date);
}

#[test]
fn test_unix_to_date_epoch_zero() {
    // Unix epoch = 1970-01-01
    let date = squeez::session::unix_to_date(0);
    assert_eq!(date, "1970-01-01", "got: {}", date);
}

#[test]
fn test_home_dir_returns_nonempty() {
    // HOME (Unix) or USERPROFILE (Windows) must be set in any real environment.
    let home = squeez::session::home_dir();
    assert!(!home.is_empty(), "home_dir() returned empty string");
}

/// Concurrent appends must never interleave a record with another's newline.
///
/// The old implementation used `writeln!`, which is `write_fmt` -- one syscall
/// for the record, another for `\n`. Under O_APPEND both are individually
/// atomic, so a second writer landing between them produced two JSON objects
/// on one line. 15 records across 12 real session files were corrupted this
/// way. Threads here stand in for the concurrent hooks (PostToolUse,
/// SubagentStop, and the wrap subprocess all append to the same file).
///
/// Asserted on line COUNT and per-line parseability rather than on file size:
/// a torn write preserves total bytes exactly, so a size check passes while
/// the file is unreadable -- which is how this survived unnoticed.
#[test]
fn test_concurrent_appends_never_split_a_record_from_its_newline() {
    let dir = std::env::temp_dir().join(format!(
        "squeez-append-race-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    const THREADS: usize = 8;
    const PER_THREAD: usize = 60;
    // Long payloads widen the window between the two syscalls the old code made.
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let payload = "x".repeat(400 + t * 7);
                    let event =
                        format!(r#"{{"type":"bash","t":{},"i":{},"pad":"{}"}}"#, t, i, payload);
                    squeez::session::append_event(&dir, "race.jsonl", &event);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let content = std::fs::read_to_string(dir.join("race.jsonl")).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(
        lines.len(),
        THREADS * PER_THREAD,
        "expected one line per append; a torn write merges two records onto one line"
    );
    for (n, line) in lines.iter().enumerate() {
        assert_eq!(
            line.matches(r#"{"type""#).count(),
            1,
            "line {} holds {} records -- a record was split from its newline: {}",
            n + 1,
            line.matches(r#"{"type""#).count(),
            &line[..line.len().min(120)]
        );
        assert!(line.starts_with('{') && line.ends_with('}'), "line {} truncated", n + 1);
    }

    std::fs::remove_dir_all(&dir).ok();
}
