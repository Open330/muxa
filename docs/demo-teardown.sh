#!/usr/bin/env bash
# Undo docs/demo-setup.sh.
#
# Split out of the tape so a render that dies halfway — vhs timeout, ^C, a
# panic in the TUI — can be cleaned up with one command instead of leaving a
# stray muxad and a labelled tmux server behind.
#
# The sandbox owns the socket, config, data directory, tmux server and shims,
# and verifies its own teardown. Only the fixture files layered on top are
# this script's to remove.
#
# Deliberately not `set -e`: every step is best-effort, and the common case is
# tearing down a partial setup where half of it never existed.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

bash "$REPO_DIR/scripts/muxa-sandbox.sh" down \
  --name muxa-demo \
  ${MUXA_DEMO_TMUX:+--tmux "$MUXA_DEMO_TMUX"}

# Fixture-only paths. The sandbox never knew about these, so it cannot have
# removed them.
rm -rf /tmp/muxa-demo-bashrc /tmp/muxa-demo-transcripts /tmp/muxa-demo-frames
rm -f /tmp/muxa-demo-config.src.toml
