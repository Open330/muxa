#!/usr/bin/env python3
"""Hold the shell fallback in `onboard.sh` to the real `muxa onboard` contract.

`muxa onboard --emit step-table` publishes the key each of the twenty steps
waits for, derived by walking the real gates. This script presses exactly those
keys at `scripts/onboard.sh --no-download` and fails if the fallback disagrees
about which step it is on — the drift that let the tour teach `t` while the
real one required `Alt-T`.

    scripts/onboarding-parity.py [--muxa target/debug/muxa] [--lang en]
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
import unicodedata

ROWS, COLS = 40, 130
STEP_TIMEOUT = 20.0
CSI = re.compile(r"\x1b\[([0-9;?]*)([@-~])")
STEP_MARK = re.compile(r"(\d+)/20")


def char_width(ch: str) -> int:
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Screen:
    """Just enough of a terminal to read the step number back off the tour."""

    def __init__(self, rows: int, cols: int) -> None:
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.pending = ""

    def clear(self) -> None:
        self.grid = [[" "] * self.cols for _ in range(self.rows)]

    def feed(self, data: str) -> None:
        data, self.pending = self.pending + data, ""
        index, size = 0, len(data)
        while index < size:
            ch = data[index]
            if ch == "\x1b":
                match = CSI.match(data, index)
                if match:
                    self._csi(match.group(1), match.group(2))
                    index = match.end()
                    continue
                # An escape sequence split across reads: keep it for next time.
                if size - index < 12:
                    self.pending = data[index:]
                    return
                index += 1
                continue
            if ch == "\r":
                self.col = 0
            elif ch == "\n":
                self.row = min(self.row + 1, self.rows - 1)
                self.col = 0
            elif ch >= " ":
                self._put(ch)
            index += 1

    def _put(self, ch: str) -> None:
        width = char_width(ch)
        if width == 0 or self.col >= self.cols:
            return
        self.grid[self.row][self.col] = ch
        for offset in range(1, width):
            if self.col + offset < self.cols:
                self.grid[self.row][self.col + offset] = ""
        self.col += width

    def _csi(self, params: str, final: str) -> None:
        if params.startswith("?"):
            return
        numbers = [int(p) if p else 0 for p in params.split(";")] if params else []

        def arg(index: int, default: int = 1) -> int:
            return numbers[index] if len(numbers) > index and numbers[index] else default

        if final in "Hf":
            self.row = min(max(arg(0) - 1, 0), self.rows - 1)
            self.col = min(max(arg(1) - 1, 0), self.cols - 1)
        elif final == "J" and (numbers[0] if numbers else 0) == 2:
            self.clear()
            self.row = self.col = 0
        elif final == "K":
            mode = numbers[0] if numbers else 0
            if mode == 0:
                for index in range(self.col, self.cols):
                    self.grid[self.row][index] = " "
            elif mode == 2:
                self.grid[self.row] = [" "] * self.cols
        elif final == "A":
            self.row = max(0, self.row - arg(0))
        elif final == "B":
            self.row = min(self.rows - 1, self.row + arg(0))
        elif final == "C":
            self.col = min(self.cols - 1, self.col + arg(0))
        elif final == "D":
            self.col = max(0, self.col - arg(0))

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.grid)


class Tour:
    def __init__(self, argv: list[str]) -> None:
        self.screen = Screen(ROWS, COLS)
        self.exited: int | None = None
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            for stale in ("TMUX", "TMUX_PANE"):
                os.environ.pop(stale, None)
            os.execvp(argv[0], argv)
            os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.set_blocking(self.fd, False)

    def pump(self, seconds: float) -> None:
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 1 << 18)
            except OSError:
                self._reap()
                return
            if not chunk:
                self._reap()
                return
            self.screen.feed(chunk.decode("utf-8", "replace"))

    def send(self, data: bytes) -> None:
        try:
            os.write(self.fd, data)
        except OSError:
            self._reap()

    def step(self) -> int | None:
        marks = STEP_MARK.findall(self.screen.text())
        return int(marks[-1]) if marks else None

    def wait_for_step(self, wanted: int) -> bool:
        deadline = time.time() + STEP_TIMEOUT
        while time.time() < deadline:
            if self.step() == wanted:
                return True
            if self.exited is not None:
                return False
            self.pump(0.2)
        return self.step() == wanted

    def _reap(self) -> None:
        if self.exited is not None:
            return
        done, status = os.waitpid(self.pid, os.WNOHANG)
        self.exited = status if done else None

    def finish(self) -> int:
        self.pump(1.0)
        deadline = time.time() + 5
        while time.time() < deadline and self.exited is None:
            self._reap()
            self.pump(0.2)
        if self.exited is None:
            os.kill(self.pid, 9)
            os.waitpid(self.pid, 0)
            self.exited = -1
        os.close(self.fd)
        return self.exited


KEY_BYTES = {
    "prefix": b"\x02",
    "Alt-T": b"\x1bt",
    "Alt-P": b"\x1bp",
    "Esc": b"\x1b",
    "Backspace": b"\x7f",
    "Enter": b"\r",
    "→": b"\x1b[C",
    "←": b"\x1b[D",
    "↑": b"\x1b[A",
    "↓": b"\x1b[B",
}


def token_bytes(token: str) -> bytes:
    if token.startswith("type:"):
        return token[len("type:") :].encode() + b"\r"
    if token in KEY_BYTES:
        return KEY_BYTES[token]
    return token.encode()


def read_contract(muxa: str) -> list[tuple[int, list[str]]]:
    raw = subprocess.run(
        [muxa, "onboard", "--emit", "step-table"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    contract = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        fields = line.split("\t")
        contract.append((int(fields[0]), fields[1:]))
    return contract


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--muxa", default=os.environ.get("MUXA_BIN", "target/debug/muxa"))
    parser.add_argument("--script", default="scripts/onboard.sh")
    parser.add_argument("--lang", default="en")
    args = parser.parse_args()

    contract = read_contract(args.muxa)
    if len(contract) != 20:
        print(f"parity: expected 20 steps from --emit step-table, got {len(contract)}")
        return 1

    tour = Tour(["sh", os.path.abspath(args.script), "--lang", args.lang, "--no-download"])
    tour.pump(1.5)

    failures = []
    for position, (number, tokens) in enumerate(contract):
        if not tour.wait_for_step(number):
            failures.append(
                f"step {number}: fallback is on {tour.step()} "
                f"(expected keys {' '.join(tokens) or '—'})"
            )
            break
        for token in tokens:
            # Pressing the chord alone cannot tell an `Alt-T` gate from a `t`
            # gate — Esc-prefixed input reaches both. Check the bare letter is
            # refused first, which is the drift that shipped in issue #76.
            bare = token[len("Alt-") :].lower() if token.startswith("Alt-") else None
            if bare:
                tour.send(bare.encode())
                tour.pump(0.35)
                if tour.step() != number:
                    failures.append(
                        f"step {number}: a bare {bare!r} advanced the fallback, but the "
                        f"real tour requires {token}"
                    )
                    break
            tour.send(token_bytes(token))
            tour.pump(0.25)
        if failures:
            break
        following = contract[position + 1][0] if position + 1 < len(contract) else None
        if following is not None and not tour.wait_for_step(following):
            failures.append(
                f"step {number}: pressing {' '.join(tokens)} left the fallback on "
                f"{tour.step()}, not {following}"
            )
            break
        print(f"  ok  {number:>2}/20  {' '.join(tokens)}")

    status = tour.finish()
    if not failures and status != 0:
        failures.append(f"fallback did not exit cleanly after the last step (status {status})")

    if failures:
        print("\nonboarding parity FAILED")
        for failure in failures:
            print(f"  - {failure}")
        print("\n`muxa onboard --emit step-table` is the contract; update scripts/onboard.sh.")
        return 1

    print(f"\nonboarding parity ok — {len(contract)} steps, same keys as muxa onboard")
    return 0


if __name__ == "__main__":
    sys.exit(main())
