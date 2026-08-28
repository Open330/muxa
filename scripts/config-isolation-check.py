#!/usr/bin/env python3
"""The sandbox server must not read the caller's `~/.tmux.conf`.

Their bindings and options would be surprising; their *hooks* are worse. muxa's
own `tmux-auto-view`, on by default since v0.8.36, binds `client-attached` and
`client-session-changed` to hand an arriving client its own session-group view
— which fires inside the sandbox the instant the learner attaches and moves
them straight back out, ending the tour at step 6 with
`[detached (from session muxa-onboarding)]`.
"""
import argparse
import pathlib
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--name", default="cfgiso")
    args = ap.parse_args()

    sandbox = HERE / "muxa-sandbox.sh"
    # A config a sandbox server would visibly obey if it read one at all.
    with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
        f.write("set -g @caller-config-was-read yes\n")
        f.write("set-hook -g client-attached 'display-message caller-hook-ran'\n")
        caller_config = f.name

    env = {**__import__("os").environ, "HOME": str(pathlib.Path(caller_config).parent)}
    # tmux reads ~/.tmux.conf; point HOME at the temp dir holding one.
    home = pathlib.Path(tempfile.mkdtemp())
    (home / ".tmux.conf").write_text(pathlib.Path(caller_config).read_text())
    env["HOME"] = str(home)
    for stale in ("TMUX", "TMUX_PANE"):
        env.pop(stale, None)

    failures = 0
    try:
        up = subprocess.run(
            ["bash", str(sandbox), "up", "--name", args.name],
            capture_output=True, text=True, env=env,
        )
        if up.returncode != 0:
            print(f"  FAIL  sandbox did not come up\n{up.stdout}\n{up.stderr}")
            return 1

        got = subprocess.run(
            ["tmux", "-S", f"/tmp/{args.name}-sandbox/tmux.sock",
             "show", "-gvq", "@caller-config-was-read"],
            capture_output=True, text=True, env=env,
        ).stdout.strip()
        ok = got != "yes"
        print(f"  {'ok  ' if ok else 'FAIL'}  the sandbox server ignored ~/.tmux.conf"
              + ("" if ok else f"   (@caller-config-was-read={got!r})"))
        failures += not ok

        # `show-hooks -g` names every hook tmux knows, set or not, so look at
        # the value rather than the name.
        bound = subprocess.run(
            ["tmux", "-S", f"/tmp/{args.name}-sandbox/tmux.sock",
             "show-hooks", "-g", "client-attached"],
            capture_output=True, text=True, env=env,
        ).stdout.strip()
        # An unset hook still prints its own name; a set one prints
        # `client-attached[0] <command>`. The command is the discriminator.
        ok = bound in ("", "client-attached")
        print(f"  {'ok  ' if ok else 'FAIL'}  no caller hook reaches the sandbox"
              + ("" if ok else f"   ({bound[:120]})"))
        failures += not ok
    finally:
        subprocess.run(["bash", str(sandbox), "down", "--name", args.name],
                       capture_output=True, text=True, env=env)

    print("the sandbox is sealed from the caller's tmux config" if not failures
          else f"{failures} failed — the caller's config leaks into the sandbox")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
