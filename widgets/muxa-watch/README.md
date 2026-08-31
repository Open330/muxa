# muxa Watch — BarShelf widget

Watch your [`muxa`](https://github.com/Open330/muxa) agents from the macOS
menu bar via [BarShelf](https://github.com/Open330/barshelf), a scriptable
menu-bar widget platform.

Each agent renders as a native two-line row — a colored state dot, where the
agent lives (`session · window`) with its last-activity time, and a caption
line with the agent kind (or, for agents waiting on you, the state in its
accent color) followed by what it is actually doing. That summary falls
through the agent's own recap → its session title → your last prompt, the same
precedence `muxa watch` uses for its SUMMARY column.

Agents running work in parallel also carry a load badge (`◇` subagents,
`▸` shells, `+` other children) and, under the row, one line per named
subagent in flight — the same glyphs as `muxa watch --view swarm`. A row whose
context window is filling up shows `ctx 84%`, in warning color past 90%.

## Sources

**Single host** (default) watches this Mac, or one SSH host if you fill in
**SSH host** with an alias from your `~/.ssh/config`. Duplicate the card per
machine.

**Fleet** shows every host your muxa controller knows about, one section per
host, from a single local `muxa fleet status --json`. The daemon already holds
each host's snapshot, so the widget itself opens no SSH connection — one card
replaces a stack of per-host ones, and hosts that are offline, degraded, or
running a mismatched muxa say so in place. Needs muxa ≥ 0.8.34 and hosts
configured under `[fleet]` in your muxa config.

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

## Compatibility

The widget reads both status payloads muxa has shipped, so a card can point at
a host running an older release than the Mac it renders on:

| muxa on the host | what the widget reads |
| --- | --- |
| ≥ 0.8.32 | the canonical `session → window → pane` topology |
| 0.8.18 – 0.8.31 | the flat `agents` array, including its `location` string |
| < 0.8.18 | nothing — `status --json` does not exist yet, and the card says so |

Because 0.8.32 replaced the payload's top-level shape *and* restarted
`schema_version` at 1 for the new tree, the widget identifies a payload by its
shape rather than its version number. Prior versions of this widget pinned the
flat shape and blanked out on that upgrade; if a future release genuinely does
carry agents this widget cannot read, the card says "Update muxa Watch"
instead of quietly rendering an empty list.
