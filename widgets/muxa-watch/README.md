# muxa Watch — BarShelf widget

Watch your [`muxa`](https://github.com/Open330/muxa) agents from the macOS
menu bar via [BarShelf](https://github.com/Open330/barshelf), a scriptable
menu-bar widget platform.

Each agent renders as a native two-line row — a colored state dot, the
agent name with its last-activity time, and a caption line with the agent
kind (or, for agents waiting on you, the state in its accent color).
Duplicate the card per host to watch several machines: leave **SSH host**
empty for this Mac, or enter an alias from your `~/.ssh/config`.

## Install

Requires [BarShelf](https://github.com/Open330/barshelf) and
[Deno](https://deno.land) (`brew install deno`).

```bash
bsf install https://github.com/Open330/muxa/tree/main/widgets/muxa-watch
```

Or the deep link (BarShelf must be installed):

```text
barshelf://install?url=https%3A%2F%2Fgithub.com%2FOpen330%2Fmuxa%2Ftree%2Fmain%2Fwidgets%2Fmuxa-watch
```

Remote hosts need muxa ≥ 0.8.18 (`status --json`).
