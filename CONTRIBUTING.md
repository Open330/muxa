# Contributing to muxa

## Toolchain

- Rust `1.89+` (workspace-pinned via `rust-version`)
- `cargo fmt`, `cargo clippy`, `cargo test` — all three are gated in CI

## Development

```bash
# build everything
cargo build --workspace

# run the full test suite
cargo test --workspace

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
2. Reinstall locally first (`cargo install --path crates/muxa-cli --force
   --locked`, same for `crates/muxad`, then restart muxad) — shipping a
   version you have not run is how a broken release gets tagged.
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

5. The Homebrew tap follows the published release through `tap-bump`, which
   needs the `TAP_GITHUB_TOKEN` secret (a fine-grained PAT with Contents
   read/write on `Open330/homebrew-tap`). Without it — including when the token
   expires — the job still *succeeds*, with a "skipping" notice, and the
   formula quietly stays behind: if `brew` keeps offering the old version after
   a release, suspect the token first. Recover with `scripts/bump-tap.sh
   vX.Y.Z`, which is idempotent and prints "nothing to push" on a current tap.

## Project layout

- `crates/muxa-core`     — types, state, config, paths, errors (no I/O)
- `crates/muxa-runtime`  — unix-socket IPC + tmux CLI wrapper
- `crates/muxa-adapters` — per-agent adapters
- `crates/muxad`         — daemon binary
- `crates/muxa`          — CLI binary

See `PROTOCOL.md` for the wire protocol spec.
