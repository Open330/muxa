#!/usr/bin/env python3
"""Drive `muxa onboard --tour live` the way a learner would, and check that the
tour follows.

The live tour advertises something specific: it never intercepts a keystroke,
so every step has to be reachable by typing the command the narration asks for.
That claim is only worth anything if something types those commands and watches
the tour keep up — which is what this does.

The narration is read back out of tmux (`show -g status-format[0]`) rather than
scraped off the screen, so a rendering change cannot make this pass or fail for
the wrong reason.

    scripts/live-tour-smoke.py [--muxa target/debug/muxa]
"""

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

SANDBOX = "muxa-onboarding"
STEP_MARK = re.compile(r"onboarding · (\d+)/(\d+)")
TOTAL_STEPS = 14


class Terminal:
    """A pty with a `muxa onboard --tour live` running in it."""

    def __init__(self, muxa: str, lang: str, no_quiz: bool = False) -> None:
        self.buffer = b""
        self.exited = False
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            # The tour refuses to nest, and CI may well run inside tmux.
            for stale in ("TMUX", "TMUX_PANE"):
                os.environ.pop(stale, None)
            argv = [muxa, "onboard", "--tour", "live", "--lang", lang]
            if no_quiz:
                argv.append("--no-quiz")
            os.execv(muxa, argv)
            os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
        os.set_blocking(self.fd, False)

    def pump(self, seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.1)
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 1 << 18)
            except OSError:
                return
            if not chunk:
                return
            self.buffer += chunk

    def type(self, text: bytes, settle: float = 2.0) -> None:
        try:
            os.write(self.fd, text)
        except OSError:
            pass
        self.pump(settle)

    def finished(self) -> bool:
        # Cached: `waitpid` reaps, so asking twice raises ChildProcessError
        # rather than answering.
        if self.exited:
            return True
        try:
            done, _ = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            self.exited = True
            return True
        self.exited = done != 0
        return self.exited

    def kill(self) -> None:
        if not self.exited:
            try:
                os.kill(self.pid, 9)
                os.waitpid(self.pid, 0)
            except OSError:
                pass
        try:
            os.close(self.fd)
        except OSError:
            pass

    def text(self) -> str:
        return self.buffer.decode("utf-8", "replace")


def tmux(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["tmux", "-L", SANDBOX, *args], capture_output=True, text=True, timeout=10
    )


def banner() -> str:
    """Row 0 belongs to tmux — its own session and window list, which the tour
    deliberately leaves alone so the learner can see what their keystrokes did."""
    return tmux("show", "-g", "status-format[1]").stdout


def step() -> int | None:
    found = STEP_MARK.search(banner())
    return int(found.group(1)) if found else None


def cue() -> str:
    return tmux("show", "-g", "status-format[3]").stdout


def title_text() -> str:
    return tmux("show", "-g", "status-format[2]").stdout


def sandbox_muxa(muxa: str, *args: str) -> str:
    """Run muxa as the learner's pane, which is the only origin the daemon
    accepts for a mailbox query."""
    env = dict(os.environ)
    for line in subprocess.run(
        ["bash", "scripts/muxa-sandbox.sh", "env", "--name", SANDBOX],
        capture_output=True, text=True,
    ).stdout.splitlines():
        if line.startswith("export ") and "=" in line:
            key, _, value = line[len("export "):].partition("=")
            env[key] = value.split(':"$PATH"')[0].strip("'")
    env["TMUX"] = env.get("MUXA_SANDBOX_TMUX_ENV", "")
    env["TMUX_PANE"] = tmux("display-message", "-p", "#{pane_id}").stdout.strip()
    return subprocess.run([muxa, *args], capture_output=True, text=True, env=env).stdout


def title_row() -> str:
    return tmux("show", "-g", "status-format[1]").stdout.strip()


def wait_step(terminal: Terminal, target: int, timeout: float = 60) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if step() == target:
            return True
        terminal.pump(0.4)
    return False


def wait_past(terminal: Terminal, target: int, timeout: float = 60) -> bool:
    """At least `target`. One skip can advance more than one step: skipping
    "detach" lands on "attach", which is already true for someone who never
    left, so the tour moves straight on."""
    end = time.time() + timeout
    while time.time() < end:
        current = step()
        if current is not None and current >= target:
            return True
        terminal.pump(0.4)
    current = step()
    return current is not None and current >= target


class Report:
    def __init__(self) -> None:
        self.failures: list[str] = []
        self.passes = 0

    def check(self, label: str, ok: bool, detail: str = "") -> bool:
        if ok:
            self.passes += 1
            print(f"  ok    {label}")
        else:
            self.failures.append(label)
            print(f"  FAIL  {label}" + (f"\n        {detail}" if detail else ""))
        return ok


def escape_hatch(muxa: str, args, report: "Report") -> int:
    """Issue #76 was a gate with no way around it. This checks the live tour
    cannot repeat that: `--no-quiz` offers the way past from the first step, and
    F12 walks the whole tour without the learner doing any of it.

    Skipping has to leave the world consistent too — the agents move into a pane
    the learner split, and there has to be one for them to move into."""
    print("escape hatch")
    terminal = Terminal(muxa, args.lang, no_quiz=True)
    try:
        terminal.pump(12)
        report.check("step 1 is showing", step() == 1, f"step={step()}")
        report.check("--no-quiz offers the way past immediately", "F12" in cue(), cue())
        report.check(
            "step 1 is legible before anyone is attached",
            "tmux new-session" in terminal.text(),
            terminal.text()[-400:],
        )

        # F12 is a tmux binding, so it only reaches the tour once the learner is
        # attached. Step 1 is the single step outside tmux, and its way past is
        # that it is a plain command.
        terminal.type(b"tmux new-session -s muxa-onboarding\r", 4)
        report.check("step 1 done for real", wait_step(terminal, 2), f"step={step()}")

        target = 3
        while target <= TOTAL_STEPS:
            terminal.type(b"\x1b[24~", 1.5)  # F12
            if not report.check(
                f"F12 reaches step {target} or past it",
                wait_past(terminal, target, 45),
                f"step={step()}",
            ):
                break
            reached = step() or target
            if reached >= 8 and target < 8:
                panes = tmux("list-panes", "-a", "-F", "#{pane_id}").stdout.split()
                report.check(
                    "the agents still had a pane to move into", len(panes) >= 3, str(panes)
                )
            target = reached + 1

        terminal.type(b"\x1b[24~", 2)
        for _ in range(10):
            if terminal.finished():
                break
            terminal.pump(2)
        report.check("the tour exits", terminal.finished())
    finally:
        terminal.kill()

    report.check("no tmux server survives", tmux("list-sessions").returncode != 0)
    strays = subprocess.run(
        ["pgrep", "-f", f"{SANDBOX}-config"], capture_output=True, text=True
    ).stdout.strip()
    report.check("no daemon survives", strays == "", strays)
    print(f"\n{report.passes} passed, {len(report.failures)} failed")
    return 1 if report.failures else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--muxa", default=os.environ.get("MUXA_BIN", "target/debug/muxa"))
    parser.add_argument("--lang", default="en")
    parser.add_argument(
        "--mode",
        choices=["tour", "skip"],
        default="tour",
        help="tour: type a learner's commands. skip: use the escape hatch instead.",
    )
    args = parser.parse_args()
    muxa = os.path.abspath(args.muxa)

    report = Report()
    if args.mode == "skip":
        return escape_hatch(muxa, args, report)

    terminal = Terminal(muxa, args.lang)
    try:
        terminal.pump(12)

        print("act I — tmux")
        report.check("1  the first step is showing", step() == 1, f"step={step()}")
        report.check("1  the cue asks for tmux new-session", "tmux new-session" in cue(), cue())
        report.check(
            "1  tmux's own status row is left alone",
            "window-status" in tmux("show", "-g", "status-format[0]").stdout,
            tmux("show", "-g", "status-format[0]").stdout[:120],
        )

        terminal.type(b"tmux new-session -s muxa-onboarding\r", 4)
        report.check("2  creating a session", wait_step(terminal, 2), f"step={step()}")
        report.check("2  the step confirms what just happened", "✓" in banner(), banner())

        terminal.type(b"\x02c", 3)
        report.check("3  a second window", wait_step(terminal, 3), f"step={step()}")

        terminal.type(b"\x02s", 2)
        terminal.type(b"q", 2)
        report.check("4  opening and closing the tree", wait_step(terminal, 4), f"step={step()}")

        terminal.type(b"\x02d", 3)
        report.check("5  detaching", wait_step(terminal, 5), f"step={step()}")
        sessions = tmux("list-sessions", "-F", "#{session_name}").stdout.split()
        report.check(
            "5  the placeholder session is not in `tmux ls`",
            "_sandbox" not in sessions,
            str(sessions),
        )

        terminal.type(b"tmux ls\r", 3)
        report.check("6  running tmux ls", wait_step(terminal, 6), f"step={step()}")

        terminal.type(b"tmux attach -t muxa-onboarding\r", 4)
        report.check("7  reattaching", wait_step(terminal, 7), f"step={step()}")

        terminal.type(b"\x02%", 3)
        report.check("8  splitting a pane", wait_step(terminal, 8), f"step={step()}")

        print("act II — muxa")
        panes = tmux("list-panes", "-a", "-F", "#{pane_id}").stdout.split()
        report.check(
            "8  the split pane became an agent, and a second joined",
            len(panes) >= 3,
            str(panes),
        )
        paths = tmux("list-panes", "-a", "-F", "#{pane_current_path}").stdout.split()
        report.check(
            "8  nothing runs outside the sandbox workspace",
            bool(paths) and all(p.startswith("/tmp/muxa-onboarding-home") for p in paths),
            str(paths),
        )
        report.check(
            "8  the windows are named after Works, not processes",
            set(tmux("list-windows", "-a", "-F", "#{window_name}").stdout.split())
            == {"checkout", "release-checks"},
            tmux("list-windows", "-a", "-F", "#{window_name}").stdout.split(),
        )

        terminal.type(b"muxa watch\r", 4)
        report.check("9  running watch", wait_step(terminal, 9), f"step={step()}")

        explains_attend = {"en": "blocked longest", "ko": "가장 오래 막힌"}[args.lang]
        report.check(
            "9  the step says what attend does", explains_attend in title_text(), title_text()
        )
        terminal.type(b"q", 2)
        terminal.type(b"muxa attend\r", 4)
        report.check("10  attend", wait_step(terminal, 10), f"step={step()}")

        explains_return = {"en": "pane you were in", "ko": "직전에 있던 pane"}[args.lang]
        report.check(
            "10  the step says what Ctrl-b ; does", explains_return in title_text(), title_text()
        )
        terminal.type(b"\x02;", 3)
        report.check("11  back in your own pane", wait_step(terminal, 11), f"step={step()}")

        terminal.type(b'muxa msg send @claude "how far along?"\r', 4)
        report.check("12  messaging a peer", wait_step(terminal, 12), f"step={step()}")

        sent = sandbox_muxa(muxa, "msg", "list", "--mailbox", "sent", "--json")
        report.check(
            "12  claude replied through muxa on its own", '"completed"' in sent, sent[-300:]
        )

        terminal.type(b"muxa msg list\r", 4)
        report.check("13  reading the mailbox", wait_step(terminal, 13), f"step={step()}")

        terminal.type(b"muxa msg inbox\r", 4)
        report.check("14  claiming the inbox", wait_step(terminal, 14), f"step={step()}")

        terminal.type(b"\x02d", 4)
        for _ in range(12):
            if terminal.finished():
                break
            terminal.pump(2)
        report.check("the tour exits on its own", terminal.finished())
        gone = {"en": "sandbox is gone", "ko": "sandbox는 사라졌습니다"}[args.lang]
        report.check("it says the sandbox is gone", gone in terminal.text(), terminal.text()[-300:])
    finally:
        terminal.kill()

    print("nothing left behind")
    report.check("no tmux server survives", tmux("list-sessions").returncode != 0)
    strays = subprocess.run(
        ["pgrep", "-f", f"{SANDBOX}-config"], capture_output=True, text=True
    ).stdout.strip()
    report.check("no daemon survives", strays == "", strays)

    print(f"\n{report.passes} passed, {len(report.failures)} failed")
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
