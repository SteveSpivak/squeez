//! `squeez compact-summary` — post-compact dense state re-injection (#166).
//!
//! Conversation history is the biggest token sink in long sessions, and
//! squeez's per-tool hooks can't reach it. After every `/compact`, Claude
//! Code loses concrete state — which files it touched, which errors it hit,
//! recent git refs — and re-discovers it with fresh tool calls.
//!
//! A PreCompact hook can't steer the built-in summarizer (researched: no
//! custom-instructions / transcript-rewrite API; it can only block). And,
//! contrary to this module's original design, a PostCompact hook can't
//! deliver `additionalContext` either: PostCompact is display-only (verified
//! against the current Claude Code hooks reference) and only supports
//! `systemMessage` / `terminalSequence` — `hookSpecificOutput.additionalContext`
//! is a UserPromptSubmit-only shape, and printing it from a PostCompact hook
//! fails the host's schema validation.
//!
//! So the PostCompact hook instead writes the summary to a pending file keyed
//! by a hashed session id, under `session::state_dir()`. A separate relay
//! (dcoder's `userprompt_guard.sh`) reads that file on the very next
//! `UserPromptSubmit` — the one event that genuinely supports
//! `additionalContext` — and deletes it. This module only ever writes; it
//! does not assume anything reads the file, so it degrades to "no re-injection
//! this compaction" rather than failing if no relay is registered.

use crate::context::cache::SessionContext;
use crate::context::retrieve;
use crate::session;

/// Build the post-compact `additionalContext` text from current session state.
/// Returns `None` when there's nothing worth re-injecting.
pub fn build_summary() -> Option<String> {
    let ctx = SessionContext::load(&session::sessions_dir());
    let cur = session::CurrentSession::load(&session::sessions_dir());

    let mut parts: Vec<String> = Vec::new();

    // Recently-touched files (newest first), with access mode.
    if !ctx.seen_files.is_empty() {
        let mut files = ctx.seen_files.clone();
        files.sort_by(|a, b| b.last_seen_call.cmp(&a.last_seen_call));
        let listed: Vec<String> = files
            .iter()
            .take(8)
            .map(|f| format!("{}({})", f.path, f.access.as_char()))
            .collect();
        parts.push(format!("files: {}", listed.join(", ")));
    }

    // Distinct error snippets (most recent first), trimmed.
    if !ctx.error_snippets.is_empty() {
        let listed: Vec<String> = ctx
            .error_snippets
            .iter()
            .rev()
            .take(3)
            .map(|(_, snip)| trim_snippet(snip))
            .collect();
        parts.push(format!("errors: {}", listed.join(" | ")));
    }

    // Recent git refs.
    if !ctx.seen_git_refs.is_empty() {
        let refs: Vec<String> = ctx.seen_git_refs.iter().rev().take(5).cloned().collect();
        parts.push(format!("git: {}", refs.join(", ")));
    }

    // Retrievable blobs for outputs compaction may have dropped. Each key is
    // annotated with its top distinctive terms (E4) so the model can tell
    // what a key is about without a squeez_retrieve round trip.
    let ids = retrieve::recent_ids(3);
    if !ids.is_empty() {
        let annotated: Vec<String> = ids
            .iter()
            .map(|id| {
                let terms = retrieve::terms_for(id, 3);
                if terms.is_empty() {
                    id.clone()
                } else {
                    format!("{}({})", id, terms.join(","))
                }
            })
            .collect();
        parts.push(format!(
            "retrievable: call squeez_retrieve with key in [{}]",
            annotated.join(", ")
        ));
    }

    if let Some(c) = cur {
        if c.tokens_saved > 0 {
            parts.push(format!(
                "squeez saved ~{}tk over {} calls this session",
                c.tokens_saved, c.total_calls
            ));
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "[squeez session state — restored after compaction] {}",
        parts.join("; ")
    ))
}

fn trim_snippet(s: &str) -> String {
    let one_line = s.replace('\n', " ");
    let t = one_line.trim();
    const MAX: usize = 80;
    if t.chars().count() <= MAX {
        t.to_string()
    } else {
        let cut: String = t.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_snippet_collapses_newlines_and_caps_length() {
        assert_eq!(trim_snippet("  a\nb  "), "a b");
        let long = "x".repeat(200);
        let out = trim_snippet(&long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 81);
    }

    #[test]
    fn safe_id_matches_known_sha256_vector() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        // first 32 hex chars -- must match the Python hashlib derivation
        // dcoder's userprompt_guard.sh relies on, byte for byte, or the
        // relay will never find the file this writes.
        if let Some(id) = safe_id("abc") {
            assert_eq!(id, "ba7816bf8f01cfea414140de5dae2223");
        }
        // else: no sha256sum/shasum on this machine's PATH -- degrades to
        // "nothing written," never to a wrong filename, so skip rather than
        // fail on an environment that can't exercise this at all.
    }

    #[test]
    fn run_with_input_returns_none_and_defers_to_pending_file_when_session_id_present() {
        // Can't easily isolate SQUEEZ_STATE_DIR from an in-process unit test
        // (global env var, parallel test execution) -- that side effect is
        // covered by the tests/test_compact_postcompact.rs integration
        // suite, which spawns the real binary per-test with its own env.
        // This test only pins the *return-value* contract: a present,
        // non-empty session_id must never surface additionalContext text on
        // stdout, because PostCompact hook stdout is what fails schema
        // validation.
        if build_summary().is_none() {
            return; // nothing to report in this test's own environment
        }
        let out = run_with_input(r#"{"session_id":"abc"}"#);
        assert!(out.is_none());
    }

    #[test]
    fn run_with_input_returns_summary_text_when_no_session_id() {
        let Some(expected) = build_summary() else {
            return; // nothing to report in this test's own environment
        };
        let out = run_with_input("{}");
        assert_eq!(out, Some(expected));
    }
}

/// Derive the same hashed filename component the UserPromptSubmit relay
/// script expects: sha256(session_id) hex, first 32 chars. Matches
/// dcoder's userprompt_guard.sh derivation (Python hashlib.sha256) byte for
/// byte — `sha256sum`/`shasum` and hashlib agree on lowercase hex digests.
fn safe_id(session_id: &str) -> Option<String> {
    let hash = crate::commands::update::compute_sha256(session_id.as_bytes())?;
    Some(hash.chars().take(32).collect())
}

/// Write the pending-context file a UserPromptSubmit relay will pick up and
/// delete on the next prompt. No-op (returns false) if hashing is
/// unavailable (no sha256sum/shasum on PATH) — callers treat that the same
/// as "nothing to re-inject," never as an error.
fn write_pending(session_id: &str, text: &str) -> bool {
    let Some(id) = safe_id(session_id) else {
        return false;
    };
    let dir = session::state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let path = dir.join(format!("postcompact.pending.{id}.json"));
    let payload = format!(
        "{{\"additionalContext\":\"{}\"}}",
        crate::json_util::escape_str(text)
    );
    std::fs::write(path, payload).is_ok()
}

/// Core logic, parameterized on the hook's stdin payload for testability.
/// Returns the plain-text summary when there's no `session_id` to key a
/// pending file by (e.g. manual `squeez compact-summary` with no piped hook
/// JSON) — callers decide whether that's worth printing.
fn run_with_input(hook_input: &str) -> Option<String> {
    let Some(text) = build_summary() else {
        return None;
    };
    match crate::json_util::extract_str(hook_input, "session_id") {
        Some(session_id) if !session_id.is_empty() => {
            write_pending(&session_id, &text);
            None
        }
        _ => Some(text),
    }
}

/// PostCompact hook entrypoint. Always exits 0 — re-injection is best-effort
/// and must never disrupt the host. Prints nothing when running as a real
/// hook (delivery happens via the pending-file relay); prints the plain
/// summary when run manually with no session id on stdin, for debugging.
pub fn run() -> i32 {
    let mut hook_input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut hook_input);

    if let Some(text) = run_with_input(&hook_input) {
        println!("{text}");
    }

    // The header tag-dedup memo (E1) tracks what the model has already seen;
    // compaction rebuilds the model's context from scratch, so the memo must
    // reset or an unchanged budget/agent tag would stay suppressed even
    // though the model no longer holds the prior header that set it.
    let sessions_dir = session::sessions_dir();
    let mut ctx = SessionContext::load(&sessions_dir);
    ctx.reset_header_tag_memo();
    ctx.save(&sessions_dir);
    0
}
