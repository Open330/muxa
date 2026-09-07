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

Nothing here waits for a duration. Every wait is a condition with a deadline,
because a fixed number of reads is a bet on how fast the runner is, and this
suite gates every pull request — one that needs a re-run to be believed has
stopped being evidence. The deadlines are far past anything a working tour
needs, so a slow runner costs seconds rather than a false failure, and a step
that genuinely never arrives still fails, saying which step it reached and what
was on screen when it gave up.

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
import threading
import time

SANDBOX_PREFIX = "muxa-onboarding"
SANDBOX = ""
STEP_MARK = re.compile(r"onboarding · (\d+)/(\d+)")
TOTAL_STEPS = 16
# What the learner's `PS1` renders. Its arrival is how the driver knows their
# shell is up and there is something to type at.
PROMPT = "muxa-onboarding $"

# How often a wait re-asks its question. Each ask costs a tmux command or two.
POLL = 0.1
# How long one step of the tour may take. Steps land in well under a second on
# an idle machine; this is sized so a runner an order of magnitude slower still
# passes, and so reaching it means the step never happened at all.
STEP_DEADLINE = 90.0
# The narration writes its three status rows one tmux call at a time, so the
# step number can change a few milliseconds before the title and cue below it
# do. Reading them is a wait, not a snapshot.
ROW_DEADLINE = 15.0
# Anything that has to cross the daemon — the mailbox, mostly — after the step
# that produced it has already opened.
DAEMON_DEADLINE = 30.0
# The tour prints its closing summary, stops its daemon, waits for the daemon
# to actually exit and deletes the sandbox, all after the last step. That is
# the slowest thing it does and it happens once, so it gets its own budget.
EXIT_DEADLINE = 120.0

# The tour polls tmux four times a second (`POLL` in `onboarding/live.rs`).
# Two steps complete on a state that opens and closes again — the session tree,
# and `muxa watch` — so the tour has to catch them *while* they are open, and
# nothing outside the tour can observe that it did. The driver therefore holds
# each one open for well past the tour's poll, which is the one place left
# where a duration is load-bearing; it is at least a multiple of a period the
# tour itself defines rather than a guess about the runner.
TOUR_POLL = 0.25
HELD_OPEN = TOUR_POLL * 8


class Terminal:
    """A pty with a `muxa onboard --tour live` running in it."""

    def __init__(self, muxa: str, lang: str, no_quiz: bool = False) -> None:
        global SANDBOX
        self.exited = False
        self._buffer = bytearray()
        self._lock = threading.Lock()
        self._stop = False
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
        # The live tour scopes every artifact to the muxa process PID. Keep the
        # driver on that same namespace so parallel smoke jobs cannot observe
        # or tear down one another's tmux server and daemon.
        SANDBOX = f"{SANDBOX_PREFIX}-{self.pid}"
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
        os.set_blocking(self.fd, False)
        # Drained on its own thread rather than by whichever check happens to
        # be running. A pty nobody reads fills up and blocks the tmux client
        # writing into it, which stalls the very tour the check is waiting on —
        # so reading is never something a check has to remember to do.
        self._reader = threading.Thread(target=self._drain, daemon=True)
        self._reader.start()

    def _drain(self) -> None:
        while not self._stop:
            try:
                ready, _, _ = select.select([self.fd], [], [], 0.2)
            except (OSError, ValueError):
                return
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 1 << 16)
            except OSError:
                return
            if not chunk:
                return
            with self._lock:
                self._buffer += chunk

    def send(self, text: bytes) -> None:
        try:
            os.write(self.fd, text)
        except OSError:
            pass

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
        self._stop = True
        self._reader.join(timeout=1)
        try:
            os.close(self.fd)
        except OSError:
            pass

    def text(self) -> str:
        with self._lock:
            return bytes(self._buffer).decode("utf-8", "replace")


def tmux(*args: str) -> subprocess.CompletedProcess:
    socket = f"/tmp/{SANDBOX}-sandbox/tmux.sock"
    return subprocess.run(
        ["tmux", "-S", socket, *args], capture_output=True, text=True, timeout=10
    )


def banner() -> str:
    """Row 0 belongs to tmux — its own session and window list, which the tour
    deliberately leaves alone so the learner can see what their keystrokes did."""
    return tmux("show", "-g", "status-format[1]").stdout


def cue() -> str:
    return tmux("show", "-g", "status-format[3]").stdout


def title_text() -> str:
    return tmux("show", "-g", "status-format[2]").stdout


def sandbox_env() -> dict[str, str]:
    """The environment a pane inside the tour runs with. PATH keeps only the
    sandbox prefix, so a lookup here proves what the *learner* would resolve."""
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
    return env


def sandbox_muxa(muxa: str, *args: str) -> str:
    """Run muxa as the learner's pane, which is the only origin the daemon
    accepts for a mailbox query."""
    return subprocess.run(
        [muxa, *args], capture_output=True, text=True, env=sandbox_env()
    ).stdout


# Every step the banner has been seen showing, and when. Printed with any
# failure: "the tour was on 9 while this waited for 8" is the whole diagnosis
# for a step that went past faster than the driver looked.
SEEN: list[tuple[float, int]] = []
STARTED = time.monotonic()
# The longest any one wait took, with its deadline: a budget quietly creeping
# towards its limit on the runner shows up before it starts failing.
SLOWEST: tuple[str, float, float] = ("", 0.0, 0.0)


def step() -> int | None:
    found = STEP_MARK.search(banner())
    if found is None:
        return None
    current = int(found.group(1))
    if not SEEN or SEEN[-1][1] != current:
        SEEN.append((round(time.monotonic() - STARTED, 2), current))
    return current


def wait(
    terminal: Terminal,
    label: str,
    ready,
    deadline: float,
    stop_on_exit: bool = True,
) -> bool:
    """Wait for a condition, never for a duration.

    Gives up at `deadline` and answers False, so a step that never arrives is
    still a failure — just not one manufactured by a busy runner.

    `stop_on_exit` answers early once the tour's process is gone, because
    nothing it was going to do will happen now. The exception is anything read
    out of the screen buffer: the last thing the tour prints is still in flight
    when it exits, so those wait for the text rather than for the process.
    """
    started = time.monotonic()
    end = started + deadline
    while True:
        if ready():
            break
        if stop_on_exit and terminal.finished() and not ready():
            return _record(label, started, deadline, False)
        if time.monotonic() >= end:
            return _record(label, started, deadline, False)
        time.sleep(POLL)
    return _record(label, started, deadline, True)


def _record(label: str, started: float, deadline: float, ok: bool) -> bool:
    global SLOWEST
    elapsed = time.monotonic() - started
    if elapsed > SLOWEST[1]:
        SLOWEST = (label, elapsed, deadline)
    return ok


def wait_step(terminal: Terminal, target: int) -> bool:
    """Wait until the tour has got to `target` — at least, not exactly.

    The step number only goes up, so "reached 8" is "is at 8 or past it". Exact
    equality made every step a race against how often this polls: one that the
    tour passes through faster than that never existed as far as the driver was
    concerned, and the wait then ran to its deadline with the tour several steps
    ahead of it. It is also simply what happens on the escape hatch, where one
    skip can advance more than one step — skipping "detach" lands on "attach",
    which is already true for someone who never left.
    """
    return wait(
        terminal,
        f"step {target}",
        lambda: (step() or 0) >= target,
        STEP_DEADLINE,
    )


def failure_detail(terminal: Terminal, extra: str = "") -> str:
    return (
        f"reached step {step()}, steps seen: {SEEN}"
        + (f"\n        {extra}" if extra else "")
        + f"\n        {terminal.text()[-600:]}"
    )


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

    def summarise(self) -> int:
        label, elapsed, deadline = SLOWEST
        if label:
            print(f"\nslowest wait: {label} — {elapsed:.1f}s of {deadline:.0f}s")
        print(f"{self.passes} passed, {len(self.failures)} failed")
        return 1 if self.failures else 0


def nothing_left_behind(report: Report) -> None:
    report.check("no tmux server survives", tmux("list-sessions").returncode != 0)
    strays = subprocess.run(
        ["pgrep", "-f", f"/tmp/{SANDBOX}-sandbox/config[.]toml"],
        capture_output=True,
        text=True,
    ).stdout.strip()
    report.check("no daemon survives", strays == "", strays)


def escape_hatch(muxa: str, args, report: "Report") -> int:
    """Issue #76 was a gate with no way around it. This checks the live tour
    cannot repeat that: `--no-quiz` offers the way past from the first step, and
    F12 walks the whole tour without the learner doing any of it.

    Skipping has to leave the world consistent too — the agents move into a pane
    the learner split, and there has to be one for them to move into."""
    print("escape hatch")
    terminal = Terminal(muxa, args.lang, no_quiz=True)
    try:
        report.check("step 1 is showing", wait_step(terminal, 1), failure_detail(terminal))
        report.check(
            "--no-quiz offers the way past immediately",
            wait(terminal, "the F12 offer", lambda: "F12" in cue(), ROW_DEADLINE),
            cue(),
        )
        report.check(
            "step 1 is legible before anyone is attached",
            wait(
                terminal,
                "step 1 on screen",
                lambda: "tmux new-session" in terminal.text(),
                ROW_DEADLINE,
            ),
            terminal.text()[-400:],
        )

        # F12 is a tmux binding, so it only reaches the tour once the learner is
        # attached. Step 1 is the single step outside tmux, and its way past is
        # that it is a plain command.
        wait(terminal, "the learner's shell", lambda: PROMPT in terminal.text(), STEP_DEADLINE)
        terminal.send(b"tmux new-session -s muxa-onboarding\r")
        report.check("step 1 done for real", wait_step(terminal, 2), failure_detail(terminal))

        target = 3
        # Step 8 is the first one that can only be reached through a split, so
        # the first time skipping arrives there is when the world it left
        # behind is worth looking at. Keyed off a flag rather than off the step
        # the loop happened to ask for: one F12 can advance more than one step,
        # and asking for exactly 8 used to mean nobody ever looked.
        looked_for_the_pane = False
        while target <= TOTAL_STEPS:
            terminal.send(b"\x1b[24~")  # F12
            if not report.check(
                f"F12 reaches step {target} or past it",
                wait_step(terminal, target),
                failure_detail(terminal),
            ):
                break
            reached = step() or target
            if reached >= 8 and not looked_for_the_pane:
                looked_for_the_pane = True
                panes = tmux("list-panes", "-a", "-F", "#{pane_id}").stdout.split()
                report.check(
                    "the agents still had a pane to move into", len(panes) >= 3, str(panes)
                )
            target = reached + 1

        terminal.send(b"\x1b[24~")
        report.check(
            "the tour exits",
            wait(terminal, "the tour exiting", terminal.finished, EXIT_DEADLINE),
            failure_detail(terminal),
        )
    finally:
        terminal.kill()

    nothing_left_behind(report)
    return report.summarise()


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
        print("act I — tmux")
        report.check(
            "1  the first step is showing", wait_step(terminal, 1), failure_detail(terminal)
        )
        report.check(
            "1  the cue asks for tmux new-session",
            wait(
                terminal,
                "step 1's cue",
                lambda: "tmux new-session" in cue(),
                ROW_DEADLINE,
            ),
            cue(),
        )
        report.check(
            "1  tmux's own status row is left alone",
            "window-status" in tmux("show", "-g", "status-format[0]").stdout,
            tmux("show", "-g", "status-format[0]").stdout[:120],
        )

        # The step is printed before the tour starts the learner's shell, so
        # having seen it is not yet having somewhere to type.
        wait(terminal, "the learner's shell", lambda: PROMPT in terminal.text(), STEP_DEADLINE)
        terminal.send(b"tmux new-session -s muxa-onboarding\r")
        report.check("2  creating a session", wait_step(terminal, 2), failure_detail(terminal))
        report.check("2  the step confirms what just happened", "✓" in banner(), banner())

        terminal.send(b"\x02c")
        report.check("3  a second window", wait_step(terminal, 3), failure_detail(terminal))

        # Opened *and* closed is one step, so the tour has to catch the tree
        # while it is open. Wait for it, then leave it there long enough that a
        # tour still polling cannot miss it.
        terminal.send(b"\x02s")
        opened = wait(
            terminal,
            "the session tree",
            lambda: "tree" in tmux("list-panes", "-a", "-F", "#{pane_mode}").stdout,
            STEP_DEADLINE,
        )
        time.sleep(HELD_OPEN)
        terminal.send(b"q")
        report.check(
            "4  opening and closing the tree",
            wait_step(terminal, 4),
            failure_detail(terminal, f"the tree was seen open: {opened}"),
        )

        terminal.send(b"\x02d")
        report.check("5  detaching", wait_step(terminal, 5), failure_detail(terminal))
        # Nobody is attached, so there is no status bar to carry the cue — and
        # bash will not re-evaluate a prompt it has already drawn. The step has
        # to print itself, or it stays invisible until the learner presses a
        # key to find out what they were supposed to press.
        report.check(
            "5  the cue is on screen without a keypress",
            wait(
                terminal,
                "step 5's printed cue",
                lambda: "tmux ls" in terminal.text(),
                ROW_DEADLINE,
            ),
            terminal.text()[-400:],
        )
        sessions = tmux("list-sessions", "-F", "#{session_name}").stdout.split()
        report.check(
            "5  the placeholder session is not in `tmux ls`",
            "_sandbox" not in sessions,
            str(sessions),
        )

        terminal.send(b"tmux ls\r")
        report.check("6  running tmux ls", wait_step(terminal, 6), failure_detail(terminal))

        terminal.send(b"tmux attach -t muxa-onboarding\r")
        report.check("7  reattaching", wait_step(terminal, 7), failure_detail(terminal))

        terminal.send(b"\x02%")
        report.check("8  splitting a pane", wait_step(terminal, 8), failure_detail(terminal))
        # The pane they split starts a shell, and that shell draws a prompt the
        # learner never asked for. Counting it satisfied this step on their
        # behalf, so the tour walked past an instruction it had just given —
        # visible here only as step 8 vanishing before the driver looked. A
        # "did not happen" cannot be waited for, so this holds for well past
        # the tour's poll and then asks.
        time.sleep(HELD_OPEN)
        report.check(
            "8  the Enter step waits for the learner, not the new pane's own prompt",
            step() == 8,
            failure_detail(terminal),
        )

        print("act II — muxa")
        # A bare Enter, not a typed command: `claude` is neither tmux nor muxa,
        # and the sandbox only pretends to have it. The tour brings the agents
        # up in response.
        terminal.send(b"\r")
        report.check(
            "9  a bare Enter brings the agents up",
            wait_step(terminal, 9),
            failure_detail(terminal),
        )

        # Everything below happens while the tour builds the fleet, and the
        # step number does not change until it has: `add_agents` returns before
        # the narration says 9, so these are settled facts, not a race.
        panes = tmux("list-panes", "-a", "-F", "#{pane_id}").stdout.split()
        report.check(
            "9  the pane they split became claude, and codex joined",
            len(panes) >= 3,
            str(panes),
        )
        # Three anonymous boxes make the mailbox steps unreadable, so each pane
        # says who it is on its own border.
        titles = tmux(
            "list-panes", "-t", "muxa-onboarding:", "-F", "#{pane_title}"
        ).stdout
        report.check(
            "9  every pane says who it is",
            "@claude" in titles and "@codex" in titles and "you" in titles,
            titles,
        )
        # The step says the agents arrived and the next one says to look at the
        # whole Work. Zooming the learner's pane hid both and left that claim
        # sitting above a blank screen.
        report.check(
            "9  the fleet is visible, not hidden behind a zoom",
            tmux("display-message", "-p", "#{window_zoomed_flag}").stdout.strip() == "0",
            tmux("display-message", "-p", "#{window_zoomed_flag}").stdout,
        )
        # The learner's pane has to fit `muxa watch` and the agents' have to fit
        # their own frames; on a 200-column window both sides clear that easily,
        # and the point of the check is that neither was starved to zero.
        visible = [
            int(w) for w in tmux(
                "list-panes", "-t", "muxa-onboarding:", "-F", "#{pane_width}"
            ).stdout.split()
        ]
        report.check(
            "9  and every pane has room to render",
            len(visible) >= 3 and min(visible) >= 60,
            str(visible),
        )
        paths = tmux("list-panes", "-a", "-F", "#{pane_current_path}").stdout.split()
        report.check(
            "9  nothing runs outside the sandbox workspace",
            bool(paths) and all(p.startswith(f"/tmp/{SANDBOX}/home") for p in paths),
            str(paths),
        )
        report.check(
            "9  the windows are named after Works, not processes",
            set(tmux("list-windows", "-a", "-F", "#{window_name}").stdout.split())
            == {"checkout", "release-checks"},
            tmux("list-windows", "-a", "-F", "#{window_name}").stdout.split(),
        )

        terminal.send(b"muxa watch\r")
        report.check("10  running watch", wait_step(terminal, 10), failure_detail(terminal))

        # Watch is the entry point the rest of muxa hangs off, so the step it
        # opens has to name its keys — the tour cannot see them pressed.
        explains_keys = {"en": "j/k move", "ko": "j/k 이동"}[args.lang]
        report.check(
            "10  the step teaches watch's keys",
            wait(
                terminal,
                "step 10's title",
                lambda: explains_keys in title_text(),
                ROW_DEADLINE,
            ),
            title_text(),
        )
        # Left, like the tree, on the same terms: this step ends when watch has
        # been seen open and then closed.
        time.sleep(HELD_OPEN)
        terminal.send(b"q")
        report.check("11  leaving watch", wait_step(terminal, 11), failure_detail(terminal))

        explains_attend = {"en": "blocked longest", "ko": "가장 오래 막힌"}[args.lang]
        report.check(
            "11  the step says what attend does",
            wait(
                terminal,
                "step 11's title",
                lambda: explains_attend in title_text(),
                ROW_DEADLINE,
            ),
            title_text(),
        )
        terminal.send(b"muxa attend\r")
        report.check("12  attend", wait_step(terminal, 12), failure_detail(terminal))

        explains_return = {"en": "pane you were in", "ko": "직전에 있던 pane"}[args.lang]
        report.check(
            "12  the step says what Ctrl-b ; does",
            wait(
                terminal,
                "step 12's title",
                lambda: explains_return in title_text(),
                ROW_DEADLINE,
            ),
            title_text(),
        )
        terminal.send(b"\x02;")
        report.check(
            "13  back in your own pane", wait_step(terminal, 13), failure_detail(terminal)
        )

        terminal.send(b'muxa msg send @claude "how far along?"\r')
        report.check("14  messaging a peer", wait_step(terminal, 14), failure_detail(terminal))

        report.check(
            "14  claude replied through muxa on its own",
            wait(
                terminal,
                "claude's reply",
                lambda: '"completed"'
                in sandbox_muxa(muxa, "msg", "list", "--mailbox", "sent", "--json"),
                DAEMON_DEADLINE,
            ),
            sandbox_muxa(muxa, "msg", "list", "--mailbox", "sent", "--json")[-300:],
        )
        # The step tells them `list` shows what came back. Reading it the way
        # they will — no `--json` — has to actually show the answer.
        answer = {"en": "regression test", "ko": "회귀 테스트"}[args.lang]
        report.check(
            "14  `msg list` shows the reply body",
            wait(
                terminal,
                "the reply body",
                lambda: answer in sandbox_muxa(muxa, "msg", "list", "--mailbox", "sent"),
                DAEMON_DEADLINE,
            ),
            sandbox_muxa(muxa, "msg", "list", "--mailbox", "sent")[-300:],
        )
        receipt = sandbox_muxa(muxa, "msg", "send", "@claude", "ping", "--no-reply")
        report.check(
            "14  `msg send` answers in one line, not a JSON dump",
            receipt.count("\n") == 1 and receipt.startswith("sent  "),
            receipt[:200],
        )

        terminal.send(b"muxa msg list\r")
        report.check(
            "15  reading the mailbox", wait_step(terminal, 15), failure_detail(terminal)
        )

        terminal.send(b"muxa msg inbox\r")
        report.check(
            "16  claiming the inbox", wait_step(terminal, 16), failure_detail(terminal)
        )

        terminal.send(b"\x02d")
        report.check(
            "the tour exits on its own",
            wait(terminal, "the tour exiting", terminal.finished, EXIT_DEADLINE),
            failure_detail(terminal),
        )
        gone = {"en": "sandbox is gone", "ko": "sandbox는 사라졌습니다"}[args.lang]
        report.check(
            "it says the sandbox is gone",
            wait(
                terminal,
                "the closing line",
                lambda: gone in terminal.text(),
                ROW_DEADLINE,
                stop_on_exit=False,
            ),
            terminal.text()[-300:],
        )
    finally:
        terminal.kill()

    print("nothing left behind")
    nothing_left_behind(report)
    return report.summarise()


if __name__ == "__main__":
    sys.exit(main())
