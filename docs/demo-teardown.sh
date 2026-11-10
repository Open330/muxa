#!/usr/bin/env bash
# Undo docs/demo-setup.sh.
#
# Split out of the tape so a render that dies halfway — vhs timeout, ^C, a
# panic in the TUI — can be cleaned up with one command instead of leaving a
# stray tendd and a labelled tmux server behind.
#
# The sandbox owns the socket, config, data directory, tmux server and shims,
# and verifies its own teardown. Only the fixture files layered on top are
# this script's to remove.
#
# Deliberately not `set -e`: every step is best-effort, and the common case is
# tearing down a partial setup where half of it never existed.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

bash "$REPO_DIR/scripts/tend-sandbox.sh" down \
  --name tend-demo \
  ${TEND_DEMO_TMUX:+--tmux "$TEND_DEMO_TMUX"}

# Fixture-only paths. The sandbox never knew about these, so it cannot have
# removed them.
rm -rf /tmp/tend-demo-bashrc /tmp/tend-demo-transcripts /tmp/tend-demo-frames
rm -f /tmp/tend-demo-config.src.toml
