# Direction

This document states what muxa is, how its parts relate, and what has to hold
before 1.0. It is a contract for scoping decisions, not a feature list; see
[README](../README.md) for the surfaces and [ARCHITECTURE](ARCHITECTURE.md) for
the runtime.

## What muxa is

**muxa is an operations platform for a team of coding agents.** It knows what
every agent is doing, gets you to the one that needs you, and runs agents as
Work: launching, coordinating, recovering, and following them across terminals
and hosts.

muxa started as a thin layer that told you which tmux agent was waiting and
jumped to it. Running many agents every day grew it into the platform it is
now. That origin survives as the Attention layer and stays a first-class,
standalone entry point.

## Two layers, both first-class

| Layer | Promise | Surfaces |
| --- | --- | --- |
| **Attention** | Nothing that needs you goes unnoticed, and getting there is one key. | hooks, `status-line`, `peek`, `watch`, `attend`, notifications, BarShelf, stats/timeline |
| **Runtime** | Agents are launched, organized, coordinated, and recovered as Work, not as loose panes. | Work/Run model, `work`/`workspace`/`agent`, muxad-owned PTYs and Muxa for Mac, collaboration mailbox and `@peer`, workers, Fleet, snapshot/restore, usage-limit recovery, MCP |

Rules that keep the layers honest:

1. **Attention never requires Runtime.** A user who only installs hooks on
   their existing panes gets the full Attention layer: no Work, no
   collaboration config, no MCP.
2. **Everything Runtime does is visible to Attention.** A worker, a peer
   request, a Fleet node, or a pipeline run that needs a human should surface
   through the same attention state, `attend`, and notifications. Today this
   holds for agent-backed state; Runtime failures without an agent row (for
   example a pipeline Run that fails before any pane exists) are not yet in
   `attend`, and closing that gap is part of this direction.
3. **Runtime never takes over what it did not start.** muxa does not kill,
   rename, or adopt unmanaged multiplexer objects as Work.

## Two execution modes

| Mode | Who starts the agent | Binding |
| --- | --- | --- |
| **Adopt** | You, in any supported multiplexer | muxa observes via hooks and provider records |
| **Own** | muxa (`muxa work`, `muxa agent`, Muxa for Mac, workers) | managed tmux pane or a muxad-owned PTY |

Both modes stay supported. Owning the process is what makes reliable prompt
delivery, recovery, and account routing possible; adopting is what keeps
muxa zero-cost to try. Standalone native (muxad-owned PTY) launches already
exist; #89 tracks the remaining rollout — host-neutral managed Work on native
execution and whether that becomes the default for **Own**. Whatever the
default, it never removes **Adopt**.

## Principles

### General product, no personal dependencies

muxa is built by heavy daily use, but it ships for everyone. Nothing in Core
or Runtime may require a tool, account layout, or workflow that only one user
has.

- Account routing and usage-limit recovery (swap-on-limit, resume-after-limit)
  solve a problem every heavy Claude/Codex user hits. They work with plain
  configuration; `aas` is one optional provider, not a prerequisite.
- No product default, preset, or code path assumes a specific organization or
  project. Personal workflows live in user config and user skills, not in
  muxa.

### Deterministic first

Every fact muxa acts on (who an agent is, which pane it is in, what state it is
in, whether a prompt landed, whether it hit a limit) is established by a
deterministic source whenever one exists:

1. hook events and provider session/transcript records,
2. provider APIs (app-server, JSON-RPC) and provider-supplied ids such as
   `CODEX_THREAD_ID`,
3. marks muxa set itself at launch (pane options, env, owned PTYs),
4. the exact process tree and its environment.

Screen detection, pane-capture parsing, and timing-based inference are
**fallbacks** for when none of those exist. The policy is that heuristic
signals are labeled as such in state and that a heuristic alone never
triggers a destructive action (stop, prune, swap, interrupt). Support for a
backend or provider is claimed only at the depth where its signals are
deterministic.

This is the target, not yet fully enforced: some current paths still act on
heuristic evidence (for example screen-prompt correlation of paneless Codex
sessions), and a unified provenance tag does not exist yet. The inventory and
conversion backlog live in
[proposals/DETERMINISTIC_FIRST.md](proposals/DETERMINISTIC_FIRST.md); new code
follows the policy from the start.

## Feature tiers

Every surface belongs to one tier. The tier decides the stability bar, not
whether the feature matters.

| Tier | Bar | Members |
| --- | --- | --- |
| **Core** | Regressions block release; live tour + tests required | Attention layer, Work/Run model, collaboration mailbox, MCP status/prompt/wait tools, daemon/IPC, `init`/`doctor`/`upgrade` |
| **Runtime** | Stable contract, documented recovery path | Fleet, workers, snapshot/restore/reload, muxad-owned PTYs, Muxa for Mac, dashboard, account routing and usage-limit recovery |
| **Labs** | Off by default or clearly marked; may change or be removed | `automation` rules beyond limit recovery, pipelines, `ask`, collaboration contracts, `graph` |

A Labs feature graduates when it has a written contract, a recovery path, and
evidence (from `muxa stats` or issues) that it is used outside a single
workflow. A feature with no use over a release cycle is a removal candidate;
the Work board replacing the card dashboard is the model.

## Before 1.0

**Safety.** muxa controls running agents, so its failures have a large blast
radius.

- `muxa upgrade` validates the live config with the new binary before
  replacing anything (#300).
- CLI/daemon protocol skew is detected and reported, never shown as
  "no active agents".
- Any rule that mutates or stops agents is dry-run against live state after
  every change, and acts only on deterministic signals.
- Release builds come from the tagged commit; the release workflow is
  restored or `scripts/release-local.sh` is the documented path.
- `main` requires green CI before merge.

**Structure.** Large single files slow review and collide when several agents
edit the tree at once.

- Split `crates/muxa-cli/src/watch.rs` by screen and `crates/muxa/src/ipc.rs`
  by request family; keep `main.rs` files thin.
- New subsystems start as modules; no source file grows past ~3,000 lines.

**Docs.** See [docs/README.md](README.md): docs describe current behavior,
proposals live under `docs/proposals/`, dated audits under `docs/validation/`.
