# R01 — `tmux::current_pane` fallback (code-quality review)

Scope: `crates/muxa/src/tmux/mod.rs:80-110` (the new `current_pane`).
Reviewer lens: staff-engineer / code quality.

Overall: a small, well-motivated, well-documented change. The doc comment
on lines 80-88 is genuinely good — it names the concrete tmux contexts
that drop `$TMUX_PANE` (`run-shell`, key bindings, `display-popup`) and
points at the in-tree recipe that triggers the bug. Most findings below
are nits or "consider for next pass" rather than blockers. No critical
issues. The one I'd actually act on before merge is the doc comment's
"focused client" claim (medium) and possibly normalising the env-var
fast path (low).

## 1. Readability / naming / complexity

### No high-severity issues.

### Doc comment slightly overstates the guarantee
**Severity:** medium
**Location:** `crates/muxa/src/tmux/mod.rs:86-88`
**Description:** The comment says the fallback "returns the pane that is
currently active in the focused client (i.e. the pane the user is
looking at behind the popup)." That is true *only when there is exactly
one client, or the spawning client is the most-recently-active one*.
`tmux display-message -p` without `-t` targets tmux's notion of the
"current" client, which is the most-recently-active client on the
server — not necessarily the client that triggered the popup. With two
attached clients on the same server (e.g. a second terminal window also
attached) this can resolve to the wrong pane. Empirically rare for a
single-user developer setup, but the doc is more confident than the
implementation warrants.
**Suggestion:** Either (a) soften the comment to "the active pane of
tmux's current client (typically the one that launched the popup)", or
(b) actually scope the query when the env tells us how. From inside a
hook/popup tmux sets `$TMUX` to `socket,pid,session_id`; you can pass
`-t '$<sid>'` to scope to that session. Option (a) is fine for now.

### `s` is reused for two different `&str`/`String` values
**Severity:** low
**Location:** `crates/muxa/src/tmux/mod.rs:103-104`
**Description:** Minor shadowing: `let s = String::from_utf8(...)?;`
then `let s = s.trim();`. Works, but reads awkwardly when scanning. The
file otherwise uses descriptive locals (`out`, `stdout`, `cols`).
**Suggestion:** `let stdout = String::from_utf8(out.stdout).ok()?;
let pane = stdout.trim();` — matches the naming in `list_panes`
(`mod.rs:72`).

### `inside_tmux()` check after the env-var check is the right order, but worth a one-liner comment
**Severity:** low
**Location:** `crates/muxa/src/tmux/mod.rs:90-95`
**Description:** A future reader might wonder why we re-check
`inside_tmux()` when the env-var path already implies "inside tmux".
The reason is correctness: we only want to spawn `tmux display-message`
when there is in fact a server to talk to (avoids a spurious "no server
running" stderr in plain shells where `$TMUX_PANE` was somehow leaked
as empty). One sentence in the body would prevent a future "simplify"
PR from collapsing it.
**Suggestion:** Add `// Only shell out when there is a server to talk
to — avoids a stray "no server running" on bare shells.`

## 2. Idiomatic Rust

### No high or critical issues.

### `var(...).ok().filter(...)` round-trips through `Result` unnecessarily
**Severity:** low
**Location:** `crates/muxa/src/tmux/mod.rs:90`
**Description:** `std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())`
is fine, but `std::env::var_os` returns `Option<OsString>` directly and
matches the existing style in `inside_tmux()` (line 77). It also avoids
allocating a `String` from a UTF-8-validated copy when the var is
absent (the common "not in tmux" case).
**Suggestion (optional):**
```rust
if let Some(p) = std::env::var_os("TMUX_PANE")
    .and_then(|v| v.into_string().ok())
    .filter(|s| !s.is_empty())
{
    return Some(p);
}
```
Not a hill to die on — the existing form is perfectly readable.

### Empty-string post-trim handling could collapse with `Option`
**Severity:** low
**Location:** `crates/muxa/src/tmux/mod.rs:104-109`
**Description:** Slightly more idiomatic to express "trim, then keep
only non-empty" as an `Option` chain.
**Suggestion (optional):**
```rust
let stdout = String::from_utf8(out.stdout).ok()?;
let pane = stdout.trim();
(!pane.is_empty()).then(|| pane.to_string())
```
Or: `Some(stdout.trim()).filter(|s| !s.is_empty()).map(str::to_owned)`.

## 3. Correctness edge cases

### No critical issues. The implementation handles the listed edges defensively.

Walking the checklist:

- **Empty stdout** — handled at `mod.rs:105-109` (returns `None`). Good.
- **Multi-line stdout** — `display-message -p '#{pane_id}'` always emits
  `<id>\n`, but `trim()` covers any future format that adds whitespace
  or a second line. If the format ever does emit two lines, the current
  code returns the *whole* trimmed multi-line blob, not just the first
  line. With the literal format `#{pane_id}` this can't happen, so this
  is purely a "future-proofing" nit.
- **Non-UTF8** — `String::from_utf8(out.stdout).ok()?` returns `None`.
  Good. (pane ids are ASCII like `%42`, so this is a theoretical concern
  only.)
- **tmux exits non-zero** — `out.status.success()` check returns `None`.
  Good. Stderr is silently dropped, which matches the "best-effort"
  contract in the doc and is consistent with `resolve_pane` (line 117).
- **tmux not in PATH** — `Command::new("tmux").output().ok()?` swallows
  the `Err(io::Error)` and returns `None`. Good. Slightly inconsistent
  with `list_panes` (line 63-74) which returns a typed `TmuxError`, but
  consistent with `resolve_pane` and the function's `Option` return
  type.

### Stderr is silently swallowed on non-zero exit
**Severity:** low
**Location:** `crates/muxa/src/tmux/mod.rs:100-102`
**Description:** When `display-message` fails (server gone between the
`inside_tmux()` check and the spawn, permission error on the socket,
etc.), the user sees a silent `None` and the watch TUI just doesn't
pre-select. Not wrong per se — fits the `Option` contract — but makes
the failure invisible during diagnosis. `list_panes` propagates stderr
via `TmuxError::NonZero` (line 68-70).
**Suggestion:** Optional: `tracing::debug!` the stderr on the failure
branch. The codebase uses `tracing` elsewhere (check
`crates/muxa-cli/src/watch.rs` for examples). This is low because
`current_pane` is best-effort and a debug log wouldn't be visible in
the popup case anyway.

### TOCTOU between `inside_tmux()` and the spawn
**Severity:** low (theoretical)
**Location:** `crates/muxa/src/tmux/mod.rs:93-99`
**Description:** A user could `tmux kill-server` between line 93 and
line 96. The spawn would then fail with "no server running" on stderr
and a non-zero status, and we'd return `None`. Already handled
correctly by the status check. Calling out explicitly so the reviewer
can confirm the team is OK with the silent-`None` behaviour in this
race. (Same race exists in `resolve_pane`.)
**Suggestion:** None — current behaviour is correct and consistent.

## 4. Test coverage

### No high or critical issues, but worth a conscious decision.

### New shell-out branch is untested; precedent in this file says that's OK
**Severity:** medium
**Location:** `crates/muxa/src/tmux/mod.rs:96-109`
**Description:** The function shells out and there's no DI seam. Looking
at the surrounding code, this is the established pattern in this crate:

- `list_panes` (`mod.rs:63-74`) — also shells out, also has zero direct
  unit tests. The *parser* `parse_pane_lines` (`mod.rs:43-61`) is
  pure, but a quick grep shows no tests cover even that (no `#[cfg(test)]`
  block in `mod.rs`).
- `scanner.rs` is the model for testable design here: the impure
  shell-out is wrapped in a closure parameter (`scan_with`, line 396)
  and the tests at `scanner.rs:392-410` inject a fake. That style would
  be overkill for a 30-line `Option<String>` helper.

So the precedent is: pure parsing helpers get unit tests; thin
shell-out wrappers don't. The change follows the precedent. The
e2e harness already pins `TMUX_PANE` (`crates/muxa-cli/tests/e2e.rs:131,174`)
which exercises the fast path; the fallback path is verified by hand
on tmux 3.4 per the brief.
**Suggestion:** Acceptable as-is. If you want belt-and-suspenders,
extract the parsing portion (post-`Output`) into a free function:
```rust
fn parse_pane_id_output(out: &std::process::Output) -> Option<String> {
    if !out.status.success() { return None; }
    let stdout = std::str::from_utf8(&out.stdout).ok()?;
    let pane = stdout.trim();
    (!pane.is_empty()).then(|| pane.to_string())
}
```
…and unit-test it with three cases (success+pane, success+empty,
non-zero exit). Worth doing only if this function grows; today it's
borderline.

## 5. Simpler alternatives the author may have missed

### Could `display-message` replace `$TMUX_PANE` entirely?
**Severity:** low (design discussion, not a defect)
**Description:** Yes, mechanically. `tmux display-message -p '#{pane_id}'`
returns a value in every context the env var does, plus the popup/hook
contexts where the env var doesn't. So the function could be:
```rust
pub fn current_pane() -> Option<String> {
    if !inside_tmux() { return None; }
    // ... shell out ...
}
```
But the env-var fast path is worth keeping for two reasons:

1. **Latency.** The brief notes `current_pane()` is hit on every
   status-line refresh. `tmux display-message` is ~1-3 ms; the env-var
   read is sub-µs. Status-line is rendered every ~1-5 s by tmux config,
   so saving 1-3 ms per refresh is real but small.
2. **Semantic difference.** `$TMUX_PANE` is the pane the *current
   process* is in; `display-message` is the pane the *focused client*
   is on. For `cmd_recap` and `cmd_status_line` invoked from inside a
   shell, `$TMUX_PANE` is the more correct answer (it's *this shell's*
   pane, not "whatever pane the user happens to be looking at right
   now"). Collapsing to display-message-only would silently change
   semantics for the common case.

**Suggestion:** Keep the two-tier design as written. Worth a short
inline comment justifying why we prefer the env var even though the
shell-out covers more cases — saves a future contributor from
"simplifying" it away. E.g.:
```rust
// Prefer the env var: it identifies *this process's* pane, whereas
// display-message returns the focused client's pane (different when
// the caller is a shell in a non-active pane).
```

### Could `command_output()`-style helper deduplicate spawn boilerplate?
**Severity:** low
**Description:** Three call sites in this crate now do the same
`Command::new("tmux").args(...).output()?; check status; from_utf8;
trim` dance (`list_panes`, `current_pane`, `scanner.rs:179`). A small
private helper `fn tmux_oneshot(args: &[&str]) -> Result<String,
TmuxError>` would shave a few lines from each. Not worth doing for
this PR; flag for the next refactor.
**Suggestion:** Defer.

---

## Summary

- **Critical:** none.
- **High:** none.
- **Medium:** doc comment overstates the "focused client" guarantee
  (mod.rs:86-88); shell-out branch has no direct test, but this matches
  the file's precedent so it's an acceptance call.
- **Low:** several stylistic tweaks (variable shadowing, `var_os`,
  silent stderr, missing one-line comments). All optional.

LGTM with the medium-severity doc tweak. Ship it.
