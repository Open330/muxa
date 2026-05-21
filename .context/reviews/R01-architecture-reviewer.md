# R01 Architecture Review — `tmux::current_pane` fallback

Reviewer lens: Principal Software Architect. Focus: layering, contracts,
single-responsibility, backward compatibility, module boundaries.

Files inspected:
- `/home/june/personal/muxa/crates/muxa/src/tmux/mod.rs` (the change)
- `/home/june/personal/muxa/crates/muxa-cli/src/watch.rs:583` (caller)
- `/home/june/personal/muxa/crates/muxa-cli/src/main.rs:380-417` (callers)
- `/home/june/personal/muxa/crates/muxa/src/tmux/scanner.rs` (sibling module)

Overall verdict: the change is **directionally correct and well documented**,
but it conflates two semantically distinct lookups behind a single `Option`
return, and silently regresses the cost model of a function that already runs
on a hot path (status-line, every ~2s). Most findings are medium/low; one
medium-leaning-high concerns the conflation of "no tmux info available" with
"tmux returned nothing" in the callers' error messages.

---

## Layering / Coupling

### Function name no longer matches its layer
**Severity:** medium
**Description:** Pre-change, `current_pane()` was a one-line `std::env::var`
read — a pure, infallible (modulo presence) accessor. Post-change
(`crates/muxa/src/tmux/mod.rs:89-110`), it spawns a child process, waits on
it, parses UTF-8, and trims output. The function now sits at the same layer
as `list_panes()` (`mod.rs:63`) — a tmux-IPC operation — but it still has
the signature and call-site ergonomics of an env-var getter. Callers like
`watch.rs:583` (`app.set_initial_pane(tmux::current_pane())`) read as if
this is free; it isn't.
**Suggestion:** Either (a) rename to `resolve_active_pane()` /
`detect_current_pane()` to signal that this is a *resolution* op that may
talk to tmux, or (b) split into two functions and let the caller compose:

```rust
pub fn current_pane_env() -> Option<String> { /* env only */ }
pub fn active_pane_via_tmux() -> Result<Option<String>, TmuxError> { /* shell-out */ }
pub fn resolve_active_pane() -> Option<String> {
    current_pane_env().or_else(|| active_pane_via_tmux().ok().flatten())
}
```

Option (b) is preferable: it keeps the trivial accessor available for tests
and hot paths, makes the shell-out explicit and observable, and lets
`cmd_status_line` opt out of the fallback if it wants to (see Backward
Compatibility below).

### Fallback policy is hard-coded in the lower layer
**Severity:** medium
**Description:** The decision "if `$TMUX_PANE` is empty, ask tmux" is a
*policy* — applicable for `watch` (interactive popup, user expects
auto-selection) but debatable for `cmd_status_line`, which runs on a tmux
status-right hook where the env var is *expected* to be set and the
fallback is at best a no-op and at worst a 5-10 ms cost per refresh
(`main.rs:380`). Putting that policy inside the lowest-level helper denies
the caller the ability to choose.
**Suggestion:** Push the policy up. Each caller already knows its
context: `watch.rs` is the one that benefits from popup-mode fallback;
`cmd_status_line` and `cmd_recap` could continue to use the cheap env read
or call `resolve_active_pane()` only when the env read returns `None` *and*
the caller deems it worth the cost.

---

## API Design

### `Option<String>` collapses three distinct outcomes
**Severity:** medium
**Description:** The new `current_pane()` returns `None` for at least
three different conditions (`mod.rs:89-110`):
1. Not inside tmux (`$TMUX` unset) — expected, terminal state.
2. Inside tmux, but `display-message` failed to spawn or exited non-zero —
   indicates a broken tmux server / PATH issue, worth surfacing.
3. Inside tmux, `display-message` returned empty/whitespace stdout —
   indicates either a tmux version regression or detached client, also
   worth surfacing.

`cmd_recap` (`main.rs:417`) currently produces `"no pane given and $TMUX_PANE
is unset"` on `None`, which is now actively misleading: the user *is* inside
tmux, the env var was empty, and the shell-out failed silently. The user
gets no clue why.
**Suggestion:** Return `Result<Option<String>, TmuxError>` (the module's
existing error type, `mod.rs:14-22`) for the shell-out path, or define a
small `enum PaneResolution { FromEnv(String), FromTmux(String), NotInTmux,
TmuxFailed(TmuxError) }`. At minimum, log a warning at the shell-out
failure site (`mod.rs:100`) so silent fallthroughs are diagnosable.

### Shell-out is invisible to callers
**Severity:** low
**Description:** Callers cannot tell whether a returned `Some(pane)` came
from the env or from tmux. That matters for retry logic (the env read is
deterministic for a process; the shell-out can be retried after, e.g., a
tmux server restart) and for telemetry. It also matters for tests —
mocking `current_pane` requires faking either the env or the entire tmux
binary.
**Suggestion:** If the split in the previous finding is adopted, this is
solved for free. Otherwise, consider returning a tagged value so callers
can log "resolved active pane via tmux fallback" at debug level.

---

## Single-Responsibility

### Function has gained two responsibilities + a heuristic
**Severity:** low
**Description:** `current_pane()` (`mod.rs:89-110`) now does:
1. Env-var read with empty-string filter.
2. `inside_tmux()` gate (a second env read).
3. Process spawn with arg construction.
4. Exit-status branch.
5. UTF-8 decode.
6. Whitespace trim with empty-after-trim guard.

That's six distinct concerns in a 22-line function. Each is correct in
isolation, but the function's name no longer describes what it does. The
doc comment is excellent (`mod.rs:80-88`) and partially compensates, but
docs aren't a substitute for a name that matches behavior.
**Suggestion:** See the split proposed under Layering. The shell-out
pipeline (steps 3-6) wants to be its own helper that returns
`Result<String, TmuxError>` and is unit-testable in isolation given a
mockable `Command` runner.

### Empty-string filter in the env branch deserves a comment
**Severity:** low
**Description:** `mod.rs:90` —
`std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())` — silently
treats an empty `$TMUX_PANE` as "unset" and falls through to the shell-out.
That's the right call (tmux's behavior in `display-popup` contexts is
exactly this), but the rationale is in the *function-level* doc, not at the
filter, where a future reader would expect it.
**Suggestion:** One-line inline comment such as `// display-popup propagates
TMUX_PANE="" — treat empty as missing` would future-proof this.

---

## Backward Compatibility

### `cmd_status_line` now shells out twice per refresh
**Severity:** medium
**Description:** `cmd_status_line` (`main.rs:380-411`) is invoked by tmux's
`status-right` hook every ~2 seconds. Pre-change, it did exactly one
shell-out: `tmux::list_panes()` at `main.rs:381`. Post-change, when
`pane: Option<String>` is `None` (the common case for status-right, which
typically calls `muxa status-line` without `--pane`), `current_pane()`
*also* shells out (`mod.rs:96`). That's a 100% increase in tmux-CLI calls
per status refresh, on the steady-state hot path.

In practice, `status-right` invocations *do* have `$TMUX_PANE` propagated
(unlike `display-popup`), so the fallback should rarely fire — but
"shouldn't fire" is not "won't fire", and silent behavior changes on hot
paths are exactly the kind of regression that's hardest to spot in
production.
**Suggestion:** (a) Confirm empirically that tmux propagates `$TMUX_PANE`
to `status-right` commands across the supported tmux versions
(3.0+? 3.4 is mentioned in the brief); (b) if confirmed, leave it but add
a regression test or comment; (c) if not, expose
`current_pane_env()` and call that from `cmd_status_line` (which only
needs the cheap path).

### `None` semantics changed for inside-tmux callers
**Severity:** low
**Description:** Pre-change, `Some(_)` meant "TMUX_PANE was set";
post-change, it can also mean "tmux told us". Any external caller (or
future caller) that relied on `None ⇔ TMUX_PANE absent` is silently
wrong now. The brief acknowledges this (focus area #4) but the codebase
itself doesn't have such a caller today, so the risk is forward-only.
**Suggestion:** Update the function doc to call out explicitly that
`Some` no longer implies the env var was set; the current doc
(`mod.rs:80-88`) explains the *why* but not the *contract change* for a
caller reading only the public API.

### `cmd_recap` error message is now stale
**Severity:** medium
**Description:** `main.rs:417` reads
`.context("no pane given and $TMUX_PANE is unset")`. Post-change, this
fires when the env was empty *and* the tmux fallback returned nothing —
e.g., tmux not on PATH, server not reachable, or `display-message`
returned empty. The user is told `$TMUX_PANE is unset`, which is no longer
the only reason.
**Suggestion:** Update to
`"no pane given and could not resolve active pane from tmux"` or similar.
This is the most user-visible regression of the change.

---

## Module Boundaries

### `Command::new("tmux")` is already this module's pattern — fine
**Severity:** low (informational)
**Description:** Confirmed: `Command::new("tmux")` is the established
pattern in this module (`mod.rs:64` for `list_panes`) and the sibling
`scanner` module (`scanner.rs:179`). Adding another invocation at
`mod.rs:96` does not introduce a new dependency or break the
"tmux-CLI-wrapper" mandate stated in the module header (`mod.rs:1-8`).
That said, `list_panes()` returns `Result<_, TmuxError>` and reports
non-zero exit + stderr, while the new shell-out swallows everything to
`Option`. The two ought to be consistent.
**Suggestion:** Have the shell-out helper return
`Result<Option<String>, TmuxError>` to match `list_panes()`'s contract,
even if the top-level `current_pane()` collapses to `Option<String>` for
caller convenience.

### Test surface unchanged but worth flagging
**Severity:** low
**Description:** No tests exist for `current_pane()` pre- or post-change
(`grep` finds no test references). The brief acknowledges this. Given the
function now has a real branch (env vs shell-out vs failure), the absence
is more felt than before.
**Suggestion:** Even without DI, the env-var path is trivially testable
with `temp-env` or `serial_test` (already common Rust patterns). The
shell-out path is harder; the standard fix is to extract a
`fn parse_display_message_output(out: &Output) -> Option<String>` and
unit-test that pure function, leaving only the `Command::new` plumbing
untested. Worth doing in a follow-up.

---

## Summary of recommended actions

1. **Highest leverage:** split `current_pane()` into
   `current_pane_env()` (cheap, env-only) + `resolve_active_pane()`
   (env-then-tmux). Lets `cmd_status_line` keep its O(1) hot path and
   makes the shell-out observable.
2. **User-visible bug:** update `cmd_recap`'s `.context(...)` message at
   `main.rs:417` — it's now misleading.
3. **Hygiene:** return `Result` from the shell-out helper to match
   `list_panes()`'s contract; extract a pure parser for testability;
   add an inline comment about the empty-string filter at `mod.rs:90`.
4. **Verify** that `$TMUX_PANE` is reliably propagated to `status-right`
   on tmux 3.0+ before declaring the per-refresh cost change benign.
