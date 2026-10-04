# Contributing to muxa

## Toolchain

- Rust `1.89+` (workspace-pinned via `rust-version`)
- `cargo fmt`, `cargo clippy`, `cargo test` — all three are gated in CI
- Node.js 22+ for the dashboard's JavaScript tests (also gated in CI)

## Development

```bash
# build everything
cargo build --workspace

# run the full test suite
cargo test --workspace

# dashboard JavaScript tests (node's built-in runner; pass the files, since
# `node --test <directory>` does not pick them up)
node --test crates/muxa/tests-js/*.test.mjs

# lint (CI-equivalent)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Running the daemon locally

```bash
# terminal A
cargo run --bin muxad

# terminal B — simulate a Claude Code hook
echo '{"session_id":"sess-1","prompt":"hi"}' \
  | cargo run --bin muxa -- hook claude --event user_prompt_submit
cargo run --bin muxa -- status
```

## Real-world topology: rules that keep biting

Most regressions in agent identity, binding and collaboration came from code
that was right for the simple test setup and wrong for real ones. Hold these
invariants:

- **A pane listing repeats a pane once per member of a session group.** Two
  terminals on one workspace (`callabo` plus `callabo~view~N`) make
  `list-panes -a` report every pane two or more times. A view row can also
  lack session-level workspace metadata. Before you count panes, require
  "exactly one" match, key anything by session, persist a session name, or do
  per-pane work (process scans, captures, IPC, typing into a pane), pass the
  listing through `muxa::tmux::canonical_panes`. Use
  `muxa::tmux::base_session_name` for durable labels. Keep raw rows only for
  actions taken on behalf of one client, such as moving that terminal's view.
- **Codex 0.160+ shares one `codex app-server` across TUIs.** Its hooks, MCP
  servers and shell commands carry the pane variables of whichever pane
  started it, so never trust `TMUX_PANE`/`RMUX_PANE` under it. Identify a
  thread by its thread id (`_meta.threadId`, `$CODEX_THREAD_ID`). Bind it
  only from evidence (see `crates/muxa/src/codex_binding.rs`).
- **Agents keep long-lived `muxa mcp` processes.** A fix to them reaches
  running agents only after the agents restart.

Before merging changes in these areas, verify them live as well as with unit
tests. The live checks must cover:

1. A grouped session with at least one `~view~` client, not only a plain
   session.
2. Two agents in the same cwd, `/new` inside a running TUI, and a muxad
   restart.
3. For each regression test: revert the fix and confirm that the test fails.
4. A throwaway `muxad` with its own `XDG_DATA_HOME`/`XDG_STATE_HOME`/
   `TMUX_TMPDIR`. Without them it writes the real history and scans the real
   multiplexer.

CI runs clippy on unpinned stable, so run `rustup update stable` before
pushing. New lints otherwise appear in CI on code you did not touch.

## Adding a new agent adapter

The three stdin-JSON adapters (`claude`, `codex`, `gemini`) all implement
the `HookAdapter` trait defined in `crates/muxa-adapters/src/hook.rs`.
To add a new agent:

1. Create `crates/muxa-adapters/src/<agent>.rs`.
2. Define `Input` (serde-deserializable payload shape) and `Event` (hook
   event enum).
3. `impl HookAdapter for <Agent>Adapter` with the three required items:
   `KIND`, `parse_event`, `normalize`.
4. Register the module in `crates/muxa-adapters/src/lib.rs`.
5. Add a `HookCmd::<Agent>` variant in `crates/muxa/src/main.rs` and wire
   it through `handle_hook`.
6. Add a hook-config snippet under `examples/`.

If the agent does **not** expose a shell-hook surface (e.g. opencode), use
the daemon HTTP bus / plugin model instead — see `adapters/opencode.rs`
for the deferred stub.

## Commit conventions

- Short imperative subject (< 72 chars), Conventional-Commits prefix
  preferred (`feat:`, `fix:`, `refactor:`, `docs:`, `chore:`, `test:`).
- Body explains **why**, not **what** — the diff shows the what.

## Releasing

1. Bump `version` in the workspace `Cargo.toml`, run a build so `Cargo.lock`
   follows, and open a `## [X.Y.Z] - date` section in `CHANGELOG.md`.
   For user-facing app changes, add highlights to `MuxaWhatsNew.catalog` in
   `apps/muxa-macos/Sources/OnboardingLogic.swift` and their Korean translations.
   Check Welcome, install docs, and landing copy against the release; regenerate
   dashboard screenshots with `scripts/landing-shots/capture.mjs` after UI changes.
2. Reinstall locally first (`cargo install --path crates/muxa-cli --force
   --locked`, same for `crates/muxad`, then restart muxad) — shipping a
   version you have not run is how a broken release gets tagged. For Mac app
   changes, run `apps/muxa-macos/Scripts/build-app.sh` without `MUXA_SKIP_EMBED`
   and verify the app's bundle version and both helpers' `--version` agree.
3. Commit as `release: vX.Y.Z`, push `main`, then push the annotated tag.

Pushing the tag is the whole trigger, and everything after it is automatic.
**Do not run `gh release create`**: the workflow creates the draft itself, the
build matrix uploads four archives into it, and only then does the `publish`
job flip the draft — with the tag's `CHANGELOG.md` section as its notes.
Creating the release by hand publishes it before the archives exist, and
`tap-bump` — which fires on *published* — dies with "no assets to download".

The run refuses to start when the tag disagrees with the tree: the workspace
version in `Cargo.toml` must equal the tag, and `CHANGELOG.md` must already
have that `## [X.Y.Z]` section. Both are cheaper to catch before four builds
than after.

4. Watch the run. It publishes on its own; you only step in when it does not:

   - a build target failed, so the release stays a draft on purpose — fix the
     target and re-push the tag, or publish the partial set deliberately;
   - the `publish` job failed — publish by hand with
     `gh release edit vX.Y.Z --draft=false --notes-file <(awk '/## \[X.Y.Z\]/{f=1;next}/^## \[/{f=0}f' CHANGELOG.md)`.

5. The Homebrew tap is synced by the release run itself, which *calls*
   `tap-bump` after publishing rather than relying on the `release: published`
   event — GitHub does not start workflows from events raised with the
   automatic `GITHUB_TOKEN`, so a publish this repo performs for itself is
   invisible to that trigger (v0.8.38 shipped with the tap a version behind
   for exactly this reason). The event trigger remains for a release a human
   publishes by hand.

   Either way it needs the `TAP_GITHUB_TOKEN` secret (a fine-grained PAT with
   Contents read/write on `Open330/homebrew-tap`). A missing token skips the
   update with a notice; an expired or invalid token fails authentication.
   In either case, check the remote formula and `muxa-app` cask versions after
   publication. Recover the CLI formula with `scripts/bump-tap.sh vX.Y.Z`,
   which is idempotent. That script does not update the Mac cask: update its
   version and DMG checksum separately, or repair the secret and rerun the
   `tap-bump` workflow for the published tag.

## Project layout

- `crates/muxa` — shared library: state, config, IPC, backends, agent adapters,
  dashboard, and notifications
- `crates/muxad` — daemon binary
- `crates/muxa-cli` — CLI binary and terminal UIs
- `crates/muxa-zellij-plugin` — zellij plugin
- `apps/muxa-macos` — native Mac app and bundled runtime build scripts
- `site` — GitHub Pages entry point using the dashboard's shared landing assets

See `PROTOCOL.md` for the wire protocol spec.
