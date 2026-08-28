#!/usr/bin/env python3
"""`--alias` must reserve the handle on the daemon the launch is talking to.

`mark_agent` registered the name against `paths::default_socket()` regardless
of `--socket` or `MUXA_SOCKET`. Against any other daemon the name is taken in a
room that knows nothing about the pane, while the room that owns it never
hears — so that room's next minted handle can hand the same name to a second
pane, and two panes answer to one alias.

Driven against a sandbox, which is the "some other daemon" case by
construction: reserve `codex` on one pane, fire a codex session start on
another, and read back what the second pane was given. `codex2` means the
sandbox daemon heard the reservation; `codex` means it did not.

Run it by hand:

    python3 scripts/alias-socket-check.py

Not wired into CI. It needs a pane to hold a running agent, and on the hosted
runner that pane closes before `mark_agent` stamps it — the launch fails with
`no such pane` before this gets anywhere near what it measures. It discriminates
reliably on a developer machine: with the fix the second pane mints `codex2`,
and with the reservation pointed back at `paths::default_socket()` it mints
`codex`. `agent_start_carries_the_callers_socket` covers the wiring in CI.
"""
import argparse
import os
import pathlib
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--name", default="aliassock")
    ap.add_argument("--muxa", default="target/debug/muxa")
    ap.add_argument("--muxad", default="target/debug/muxad")
    args = ap.parse_args()

    muxa = str(pathlib.Path(args.muxa).resolve())
    muxad = str(pathlib.Path(args.muxad).resolve())
    sock = f"/tmp/{args.name}-sandbox/tmux.sock"
    script = str(HERE / "muxa-sandbox.sh")

    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}

    # A stand-in `codex`: CI has none, and a pane whose command cannot exec
    # closes before its metadata is stamped.
    fake_bin = pathlib.Path(tempfile.mkdtemp())
    fake = fake_bin / "codex"
    fake.write_text("#!/bin/sh\nexec sleep 600\n")
    fake.chmod(0o755)

    def sandbox(*argv: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", script, *argv, "--name", args.name],
                              capture_output=True, text=True, env=env)

    def tmux(*argv: str, tenv=None) -> str:
        return subprocess.run(["tmux", "-S", sock, *argv], capture_output=True,
                              text=True, env=tenv or env).stdout

    failures = 0
    try:
        up = sandbox("up", "--muxa", muxa, "--muxad", muxad,
                     "--extra-path", str(fake_bin))
        if up.returncode != 0:
            print(f"  FAIL  sandbox did not come up\n{up.stdout}\n{up.stderr}")
            return 1
        if sandbox("daemon").returncode != 0:
            print("  FAIL  sandbox daemon did not start")
            return 1

        senv = dict(env)
        for line in sandbox("env").stdout.splitlines():
            if line.startswith("export ") and "=" in line:
                key, _, value = line[len("export "):].partition("=")
                senv[key] = value.split(':"$PATH"')[0].strip("'")
        senv["TMUX"] = senv.get("MUXA_SANDBOX_TMUX_ENV", "")

        # The launch resolves `codex` inside a pane, from the tmux server's
        # environment rather than this process's. `--extra-path` only prepends
        # to what `sandbox env` prints for the caller, so the server — and
        # every pane it spawns — still finds the real one. Put the stand-in on
        # the server's PATH, and check it landed in front: a real `codex`
        # winning would mean this check starts agents on somebody's machine
        # instead of measuring handle arbitration.
        tmux("set-environment", "-g", "PATH",
             f"{fake_bin}:{senv.get('PATH', '')}", tenv=senv)
        server_path = tmux("show-environment", "-g", "PATH", tenv=senv).strip()
        if not server_path.startswith(f"PATH={fake_bin}:"):
            print(f"  FAIL  the stand-in codex is not first on the server PATH\n        {server_path[:160]}")
            return 1
        print("  ok    the stand-in codex leads the tmux server's PATH")

        tmux("new-session", "-d", "-s", "aliascheck", tenv=senv)
        tmux("split-window", "-d", "-t", "aliascheck:", tenv=senv)
        panes = tmux("list-panes", "-t", "aliascheck:", "-F", "#{pane_id}", tenv=senv).split()
        held, other = panes[0], panes[1]

        started = subprocess.run(
            [muxa, "agent", "start", "--agent", "codex", "--alias", "codex",
             "--placement", "pane", "--target", held, "--json"],
            capture_output=True, text=True, env=senv,
        )
        if started.returncode != 0:
            print(f"  FAIL  `agent start --alias` did not run\n        {started.stderr.strip()[:200]}")
            return 1
        print("  ok    `agent start --alias codex` succeeded")

        subprocess.run([muxa, "hook", "codex", "--event", "session_start"],
                       input='{"session_id":"alias-socket-check"}',
                       capture_output=True, text=True,
                       env=dict(senv, TMUX_PANE=other))
        minted = ""
        for _ in range(40):
            time.sleep(0.4)
            minted = tmux("show", "-p", "-t", other, "-v", "@muxa_agent_alias", tenv=senv).strip()
            if minted:
                break

        ok = bool(minted) and minted != "codex"
        print(f"  {'ok  ' if ok else 'FAIL'}  the sandbox daemon knew `codex` was taken"
              + (f"   (second pane minted {minted!r})" if minted else "   (nothing minted)"))
        failures += not ok
    finally:
        sandbox("down")

    print("the alias reservation follows the launch's daemon" if not failures
          else f"{failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
