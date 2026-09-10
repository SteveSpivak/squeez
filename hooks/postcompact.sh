#!/usr/bin/env bash
# squeez PostCompact hook — stages session state for re-injection after
# context compaction.
#
# PostCompact fires after Claude Code compacts the context window. Compaction
# can drop concrete state (files touched, errors hit, git refs). squeez
# already tracks that -- but PostCompact is a display-only event (Claude Code
# hooks reference: only `systemMessage`/`terminalSequence` are supported
# there, not `additionalContext`), so this hook cannot inject it directly.
# `squeez compact-summary` instead writes a pending file keyed by this
# session's hashed id; a UserPromptSubmit-side relay (dcoder's
# userprompt_guard.sh, or any hook honoring the same pending-file contract)
# delivers it on the next prompt, via the event that actually supports
# `additionalContext`.
set -euo pipefail

SQUEEZ="$HOME/.claude/squeez/bin/squeez"
if [ ! -x "$SQUEEZ" ]; then
    _sq=$(command -v squeez 2>/dev/null || true)
    [ -n "$_sq" ] && SQUEEZ="$_sq"
fi
[ ! -x "$SQUEEZ" ] && exit 0

# Capture the hook's JSON payload (contains session_id) once, up front --
# `squeez track` doesn't touch stdin, but reading it only after that call
# would be fragile against future changes to that command.
hook_input="$(cat 2>/dev/null || true)"

"$SQUEEZ" track PostCompact 0 2>/dev/null || true

printf '%s' "$hook_input" | "$SQUEEZ" compact-summary 2>/dev/null || true
