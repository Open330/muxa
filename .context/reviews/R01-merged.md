# Code Review Summary (R01) — `tmux::current_pane` fallback

Reviewers run: code-quality (Claude), architecture (Claude). Codex skipped (auth 401).

## Critical / High
- None.

## Medium — addressed in this round

### Multi-client correctness — `display-message` without `-t` returns wrong client
Both reviewers flagged that `tmux display-message` defaults to the
"most-recently-active" client; with multiple attached clients this can
return the active pane of a *different* session than the one that
triggered the binding. The original doc comment also overstated the
guarantee.

**Fix applied** (`crates/muxa/src/tmux/mod.rs`):
- Parse the session id out of `$TMUX` (`socket,pid,session_id`).
- Pass `-t '$<sid>'` to `display-message`, scoping the query to the
  session that actually triggered the popup.
- Added a pure helper `parse_tmux_session_target` with two unit tests
  (well-formed env / malformed inputs).
- Doc comment rewritten to describe the actual guarantee.

### Stale error message in `cmd_recap`
`"no pane given and $TMUX_PANE is unset"` was misleading after the
fallback was added — the env var being unset is no longer the only
failure path.

**Fix applied** (`crates/muxa-cli/src/main.rs:417`): changed to
`"no pane given and could not determine current tmux pane"`.

## Medium — acknowledged, not changed

### Function name no longer matches its layer (architecture-reviewer)
`current_pane()` was a pure env read; it now does an IPC-style shell-out.
A future split into `current_pane_env()` + `resolve_active_pane()` could
make the cost observable at call sites and let `cmd_status_line` opt out
of the fallback. Decision: defer — the function still has a single
caller-visible contract ("best-effort active pane") and the cost is
well-bounded (one extra `tmux` exec only when `$TMUX_PANE` is empty,
which is rare for status-line refreshes).

### `cmd_status_line` shell-out cost (architecture-reviewer)
Status-line `#(...)` substitutions inherit `$TMUX_PANE` from the pane
they render in, so the fallback path won't fire on the hot path. Verified
empirically that `TMUX_PANE` *is* set in pane shells; only popup/run-shell
spawn contexts are missing it.

### `Option<String>` collapses three outcomes (architecture-reviewer)
True, but no current caller distinguishes "not in tmux" from "shell-out
failed" — they all land on row 0 / a "no pane" error. Restructuring the
return type would be churn without a concrete consumer.

## Low — acknowledged, not changed
- Stderr from the fallback `tmux` call is silently dropped. Could be a
  `tracing::debug!` later if we ever need to triage failures.
- Minor variable-shadow style cleanup (`s` → `s.trim()`); rewritten
  during the multi-client fix anyway, no longer relevant.

## Statistics
- Total findings: 8 (CQ: 4, Arch: 8, with overlap)
- Acted on: 2 medium
- Acknowledged: 4 medium / low
- New unit tests: 2 (`parse_tmux_session_target` happy + malformed paths)

## Final test status
- `cargo build --workspace` — clean
- `cargo test --workspace` — 139 passed, 0 failed (was 137; +2 new)
- `cargo clippy --workspace` — clean
- `cargo fmt --check` — clean
- Empirical verification: `tmux run-shell` with `TMUX_PANE=""` now
  resolves the user's active pane via the scoped fallback.
