# `docs/demo.gif` — how to regenerate

The hero GIF embedded in the project README is checked into the repo
rather than generated at build time. Whenever you change the visible
flow (new keybinds, layout shifts, a renamed subcommand) you'll want
to re-record it.

## Files

| File                   | Role                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------- |
| `docs/demo.tape`       | [VHS](https://github.com/charmbracelet/vhs) script — the recording itself.            |
| `docs/demo-setup.sh`   | Bootstraps an isolated `tmux -L muxa-demo` server with a few seeded panes + windows.  |
| `docs/demo-seed.sh`    | Older pane-seed script (kept for reference; the live setup script does this inline).  |
| `docs/demo.gif`        | The output. ~230 KB, 1200 × 720, ~17 s.                                               |

The tape's prelude (`Hide` block) does the boring setup so the visible
recording stays focused on the muxa UI:

1. Spawns a fresh `muxad` against `MUXA_SOCKET=/tmp/muxa-demo.sock`.
2. Runs `docs/demo-setup.sh`, which stands up the labelled tmux server
   with three seeded agent panes (Claude / Codex / Gemini) plus a few
   bare panes for variety, and binds `prefix + s` to the watch popup.
3. `exec`s into `tmux -L muxa-demo attach -t main:0` so the recording
   opens already inside the demo session.

## Happy path — when Chrome can sandbox

If you're on a desktop or any environment where headless Chrome can
create a user namespace:

```bash
cd /path/to/muxa
vhs docs/demo.tape
```

VHS handles the rest: spins up `ttyd`, points headless Chrome at it,
records the rendered terminal, encodes the GIF. Drop the resulting
`docs/demo.gif` straight into the commit.

## Fallback — when Chrome can't sandbox

In sandboxed dev environments (some container-in-container setups,
restricted user namespaces, locked-down CI runners), Chrome bails with
something like:

```
could not launch browser: Failed to move to new namespace:
PID namespaces supported, Network namespace supported, but failed:
errno = Operation not permitted
```

VHS doesn't expose `--no-sandbox` to the user, so the workaround is to
record inside the upstream VHS Docker image where Chrome's namespace
needs are met:

```bash
# 1) Bring up a long-lived shell container off the official image.
CID=$(docker create --entrypoint sh ghcr.io/charmbracelet/vhs:latest \
        -c "mkdir -p /work/docs && sleep 600")
docker start "$CID"

# 2) Install tmux — the image ships vhs/chromium/ttyd/ffmpeg but not tmux.
docker exec "$CID" sh -c \
  "apt-get update -qq >/dev/null && apt-get install -y -qq tmux >/dev/null"

# 3) Inject the host's muxa binaries + the demo files. Bind mounts are
#    unreliable when the docker daemon itself is containerized, so use
#    `docker cp` instead.
docker cp ~/.cargo/bin/muxa  "$CID":/usr/local/bin/muxa
docker cp ~/.cargo/bin/muxad "$CID":/usr/local/bin/muxad
docker cp docs/demo.tape       "$CID":/work/docs/demo.tape
docker cp docs/demo-setup.sh   "$CID":/work/docs/demo-setup.sh
docker cp docs/demo-seed.sh    "$CID":/work/docs/demo-seed.sh

# 4) Record. ~17 s of visible content + a few seconds of setup/teardown.
docker exec -w /work "$CID" vhs docs/demo.tape

# 5) Pull the resulting GIF out and tear the container down.
docker cp "$CID":/work/docs/demo.gif docs/demo.gif
docker rm -f "$CID"
```

The host muxa binaries need to be ABI-compatible with the container
(Debian Trixie / glibc 2.40-ish). A release build from this repo on a
recent Ubuntu / Debian works out of the box; if you're on an older
distro, build inside the container instead:

```bash
# inside the container, before step 4
docker exec "$CID" sh -c \
  "apt-get install -y -qq curl build-essential >/dev/null && \
   curl -sSf https://sh.rustup.rs | sh -s -- -y >/dev/null"
docker cp . "$CID":/src
docker exec -w /src "$CID" sh -c \
  ". $HOME/.cargo/env && \
   cargo install --path crates/muxa-cli --locked && \
   cargo install --path crates/muxad --locked"
```

## Tweaking the tape

A few patterns that come up:

* **Pacing**: the visible flow targets ~17 s. `Sleep` durations are in
  milliseconds; ~1500 ms after a key gives the viewer time to read,
  ~500 ms is right between consecutive keystrokes.
* **New key in muxa watch**: add the keystroke after `Type "p"` /
  `Type "j"` blocks. Keep the prelude untouched — it's already
  carrying `prefix + s` keybind, status bar wiring, and the
  three-agent seed.
* **Higher resolution**: bump `Set Width` / `Set Height`. The README
  embed scales to its container width; 1200 × 720 lands cleanly on
  most monitors and keeps the file under 300 KB.
* **Theme**: `Set Theme "TokyoNight"` matches the muxa colour palette;
  see `vhs themes` for alternatives.

## Troubleshooting

| Symptom                                                                  | Cause                                                          | Fix                                                                  |
| ------------------------------------------------------------------------ | -------------------------------------------------------------- | -------------------------------------------------------------------- |
| `Failed to move to new namespace: Operation not permitted`               | Chrome's sandbox can't run in the current env.                 | Use the Docker fallback above.                                       |
| `tmux: command not found` inside the recording                           | The container doesn't have tmux installed.                     | Step 2 of the fallback (`apt-get install tmux`).                     |
| `muxa: command not found` inside the recording                           | The container can't see the host's muxa binaries.              | Step 3 of the fallback (`docker cp` muxa + muxad to `/usr/local/bin`). |
| Gif is generated but the agent rows are empty                            | `docs/demo-setup.sh` failed silently — usually muxad isn't up. | Re-run with the prelude's `>/dev/null` removed and read the output.  |
| `parser: N error(s)` from VHS                                            | A typo in the tape — usually a stray paren or an absolute path treated as a command. | `vhs validate docs/demo.tape` and read the highlighted spans.        |
