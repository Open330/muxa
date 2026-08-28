#!/usr/bin/env python3
"""`--alias` must reserve the handle on the daemon the launch is talking to.

The reservation used `paths::default_socket()` regardless of `--socket`, so a
launch against any other daemon reserved the name in the wrong room — and on a
host with no daemon on the default socket at all, the reservation failed, the
error propagated out of `mark_agent`, and the whole launch was rolled back.

Driven against a sandbox, which is exactly the "some other daemon" case.
"""
import argparse
import os
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent


def sandbox(name: str, *args: str, env=None) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(HERE / "muxa-sandbox.sh"), *args, "--name", name],
        capture_output=True, text=True, env=env,
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--name", default="aliassock")
    ap.add_argument("--muxa", default="target/debug/muxa")
    ap.add_argument("--muxad", default="target/debug/muxad")
    args = ap.parse_args()

    muxa = str(pathlib.Path(args.muxa).resolve())
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
    # No daemon on the default socket: the old code had nothing to reserve
    # against and failed the launch outright.
    env["XDG_RUNTIME_DIR"] = str(pathlib.Path(subprocess.run(
        ["mktemp", "-d"], capture_output=True, text=True).stdout.strip()))

    failures = 0
    try:
        up = sandbox(args.name, "up", "--muxa", muxa,
                     "--muxad", str(pathlib.Path(args.muxad).resolve()), env=env)
        if up.returncode != 0:
            print(f"  FAIL  sandbox did not come up\n{up.stdout}\n{up.stderr}")
            return 1
        if sandbox(args.name, "daemon", env=env).returncode != 0:
            print("  FAIL  sandbox daemon did not start")
            return 1

        sandbox_env = dict(env)
        for line in sandbox(args.name, "env", env=env).stdout.splitlines():
            if line.startswith("export ") and "=" in line:
                key, _, value = line[len("export "):].partition("=")
                sandbox_env[key] = value.split(':"$PATH"')[0].strip("'")
        sandbox_env["TMUX"] = sandbox_env.get("MUXA_SANDBOX_TMUX_ENV", "")

        # A session of its own, so the launch has a window to place into.
        sock = f"/tmp/{args.name}-sandbox/tmux.sock"
        subprocess.run(["tmux", "-S", sock, "new-session", "-d", "-s", "aliascheck"],
                       capture_output=True, text=True, env=sandbox_env)
        pane = subprocess.run(
            ["tmux", "-S", sock, "list-panes", "-t", "aliascheck:", "-F", "#{pane_id}"],
            capture_output=True, text=True, env=sandbox_env,
        ).stdout.split()[0]
        # Reserve `codex` on one pane, then let the *other* pane mint a
        # default handle from a codex session start. The sandbox daemon
        # arbitrates that namespace: if it heard the reservation it hands the
        # second pane `codex2`, and if it never heard it — because the
        # reservation went to whatever daemon `default_socket()` names — it
        # hands out `codex` again and two panes answer to one name.
        sock = f"/tmp/{args.name}-sandbox/tmux.sock"
        subprocess.run(["tmux", "-S", sock, "new-session", "-d", "-s", "aliascheck"],
                       capture_output=True, text=True, env=sandbox_env)
        subprocess.run(["tmux", "-S", sock, "split-window", "-d", "-t", "aliascheck:"],
                       capture_output=True, text=True, env=sandbox_env)
        panes = subprocess.run(
            ["tmux", "-S", sock, "list-panes", "-t", "aliascheck:", "-F", "#{pane_id}"],
            capture_output=True, text=True, env=sandbox_env,
        ).stdout.split()
        held, other = panes[0], panes[1]

        started = subprocess.run(
            [muxa, "agent", "start", "--agent", "codex", "--alias", "codex",
             "--placement", "pane", "--target", held, "--json"],
            capture_output=True, text=True, env=sandbox_env,
        )
        if started.returncode != 0:
            print(f"  FAIL  `agent start --alias` did not run\n        {started.stderr.strip()[:200]}")
            return 1
        print("  ok    `agent start --alias codex` succeeded")

        hook_env = dict(sandbox_env, TMUX_PANE=other)
        subprocess.run(
            [muxa, "hook", "codex", "--event", "session_start"],
            input='{"session_id":"alias-socket-check"}',
            capture_output=True, text=True, env=hook_env,
        )
        import time
        minted = ""
        for _ in range(30):
            time.sleep(0.4)
            minted = subprocess.run(
                ["tmux", "-S", sock, "show", "-p", "-t", other, "-v", "@muxa_agent_alias"],
                capture_output=True, text=True, env=sandbox_env,
            ).stdout.strip()
            if minted:
                break

        ok = minted != "codex"
        print(f"  {'ok  ' if ok else 'FAIL'}  the sandbox daemon knew `codex` was taken"
              + (f"   (second pane minted {minted!r})" if minted else "   (nothing minted)"))
        failures += not ok

    finally:
        sandbox(args.name, "down", env=env)

    print("the alias reservation follows the launch's daemon" if not failures
          else f"{failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
