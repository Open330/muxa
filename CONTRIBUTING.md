# Contributing to muxa

## Toolchain

- Rust `1.88+` (workspace-pinned via `rust-version`)
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

## Project layout

- `crates/muxa-core`     — types, state, config, paths, errors (no I/O)
- `crates/muxa-runtime`  — unix-socket IPC + tmux CLI wrapper
- `crates/muxa-adapters` — per-agent adapters
- `crates/muxad`         — daemon binary
- `crates/muxa`          — CLI binary

See `PROTOCOL.md` for the wire protocol spec.
