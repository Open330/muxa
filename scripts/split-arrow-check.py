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
sys.argv = ["parity"]
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--muxa", default="target/debug/muxa")
    ap.add_argument("--gaps", default="1,20,45", help="split delays in ms")
    args = ap.parse_args()

    failures = 0
    whole = probe(args.muxa, None)
    print(f"  {'ok  ' if whole else 'FAIL'}  arrow in one write")
    failures += not whole
    for gap in [int(g) for g in args.gaps.split(",")]:
        torn = probe(args.muxa, gap)
        print(f"  {'ok  ' if torn else 'FAIL'}  arrow split across two writes, {gap}ms apart")
        failures += not torn

    print("split arrow ok — step 6 accepts a torn escape sequence" if not failures
          else f"{failures} failed — step 6 drops the arrow it is the only gate for")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
