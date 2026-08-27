#!/usr/bin/env python3
"""Step 6 takes an arrow and nothing else. It must accept one that arrives torn.

Issue #76 was a gate with no way past. The split-escape handler added to close
it swallowed the CSI tail and returned nothing, which drops the arrow along
with the phantom `Esc` — the same dead end, on the one step whose contract has
no letter equivalent. A terminal that ships `\\x1b` and `[C` in separate writes
is not exotic; it is what the original report described.

Held against the real tour, driven with the keys `--emit step-table` publishes,
and read from the tour's own step counter rather than from screen churn.
"""
import argparse
import importlib.util
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("parity", HERE / "onboarding-parity.py")
parity = importlib.util.module_from_spec(spec)
# The parity harness parses `sys.argv` at import. Hand it none and keep our own,
# or every flag below is silently ignored and the check runs on its defaults.
ARGV, sys.argv = sys.argv[1:], [sys.argv[0]]
spec.loader.exec_module(parity)

ARROW_STEP = 6


def reach_arrow_step(muxa: str) -> "parity.Tour":
    """Drive the published keys up to — but not through — the arrow gate."""
    tour = parity.Tour([muxa, "onboard", "--lang", "en"])
    tour.pump(1.0)
    for number, tokens in parity.read_contract(muxa):
        if number == ARROW_STEP:
            if not tour.wait_for_step(ARROW_STEP):
                tour.close()
                raise SystemExit(f"could not reach step {ARROW_STEP} (at {tour.step()})")
            return tour
        for token in tokens:
            tour.send(parity.token_bytes(token))
            tour.pump(0.25)
    tour.finish()
    raise SystemExit(f"step {ARROW_STEP} is no longer the arrow gate; update this check")


def probe(muxa: str, gap_ms: int | None) -> bool:
    """True when the arrow advanced the tour. `gap_ms=None` sends it whole."""
    tour = reach_arrow_step(muxa)
    try:
        if gap_ms is None:
            tour.send(b"\x1b[C")
        else:
            tour.send(b"\x1b")
            tour.pump(gap_ms / 1000)
            tour.send(b"[C")
        return tour.wait_for_step(ARROW_STEP + 1)
    finally:
        tour.finish()


def advances(muxa: str, gap_ms: int | None, attempts: int) -> bool:
    """Whether the arrow ever gets through.

    Driving a TUI over a pty is timing-sensitive: reaching the gate, or seeing
    the step counter move, times out perhaps one run in ten under load. Retry
    rather than ship a flaky gate — the signal survives it, because a binary
    that drops the key fails every attempt, not one in ten.
    """
    return any(probe(muxa, gap_ms) for _ in range(attempts))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--muxa", default="target/debug/muxa")
    ap.add_argument("--attempts", type=int, default=3, help="retries before failing")
    # Below about 5 ms the two writes coalesce in the pty often enough to be
    # nondeterministic, which exercises the *un*-split path and makes the check
    # flaky rather than strict. Real split escapes are milliseconds apart.
    ap.add_argument("--gaps", default="10,20,45", help="split delays in ms")
    args = ap.parse_args(ARGV)

    failures = 0
    whole = advances(args.muxa, None, args.attempts)
    print(f"  {'ok  ' if whole else 'FAIL'}  arrow in one write")
    failures += not whole
    for gap in [int(g) for g in args.gaps.split(",")]:
        torn = advances(args.muxa, gap, args.attempts)
        print(f"  {'ok  ' if torn else 'FAIL'}  arrow split across two writes, {gap}ms apart")
        failures += not torn

    print("split arrow ok — step 6 accepts a torn escape sequence" if not failures
          else f"{failures} failed — step 6 drops the arrow it is the only gate for")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
