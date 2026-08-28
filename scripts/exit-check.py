#!/usr/bin/env python3
"""`exit` in the learner's pane must end the tour, not print tmux's error.

Typing `exit` closes the pane, and with it the last window, the session and the
sandbox server. The tour polls that server several times a second, so a poll
landing after the server is gone used to surface tmux's own
`no server running on …` at somebody who had just typed `exit`.
"""
import argparse
import importlib.util
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("smoke", HERE / "live-tour-smoke.py")
smoke = importlib.util.module_from_spec(spec)
ARGV, sys.argv = sys.argv[1:], [sys.argv[0]]
spec.loader.exec_module(smoke)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--muxa", default="target/debug/muxa")
    ap.add_argument("--lang", default="en")
    args = ap.parse_args(ARGV)

    terminal = smoke.Terminal(args.muxa, args.lang)
    failures = 0
    try:
        terminal.pump(1.0)
        # Far enough in to be attached, with a session the `exit` can close.
        terminal.type(b"tmux new-session -s muxa-onboarding\r", 4)
        if not smoke.wait_step(terminal, 2):
            print(f"  FAIL  could not reach step 2 (at {smoke.step()})")
            return 1

        terminal.type(b"exit\r", 3)
        for _ in range(12):
            if terminal.finished():
                break
            terminal.pump(2)

        text = terminal.text()
        checks = [
            ("the tour exits", terminal.finished(), text[-400:]),
            ("no raw tmux error reaches the learner", "no server running" not in text, text[-400:]),
            ("it still says the sandbox is gone", "sandbox is gone" in text, text[-400:]),
        ]
        for name, passed, detail in checks:
            print(f"  {'ok  ' if passed else 'FAIL'}  {name}")
            if not passed:
                failures += 1
                print(f"        {detail!r}")
    finally:
        terminal.kill()

    print("exit is a clean way out" if not failures else f"{failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
