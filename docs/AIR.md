# muxa and AIR

[AIR](https://github.com/jiunbae/air) is the Agent Intermediate
Representation: a versioned JSON project format (`workflow`, `plan`, `trace`)
with schemas, a specification, and a protocol manifest that two
implementations negotiate before they exchange artifacts. AIR Workbench is
its local-first editor — a React Flow canvas over `SKILL.md` that preserves
the source bytes, discovers installed Skills, and shows metadata-only session
evidence.

muxa is a runtime. It owns tmux, starts agent CLIs, converges a Work's panes,
wakes agents through a mailbox, and records what happened. Its pipeline lives
in `config.toml` as `[pipeline.<name>]`.

The two do not overlap: **AIR describes, muxa runs.** This document is the
plan for making them fit without either one swallowing the other.

## The judgment

**Adopt AIR as an interchange and review format. Do not adopt it as muxa's
internal model, and do not take a runtime dependency on it.**

Three reasons.

1. **The formats answer different questions.** A muxa pipeline says which
   agent CLI occupies which pane, which alias waits on which, and what each
   is told. An AIR workflow says what the steps are and how they depend on
   each other, in a way another tool can read. Everything muxa needs to
   launch a pane (program, alias, split direction, layout, `after`,
   worktrees, routes) is runtime detail that AIR deliberately does not model
   — it has an extension mechanism for exactly this kind of vendor payload.
2. **The runtimes do not mix.** muxa is a Rust daemon that has to work on a
   headless Linux host over SSH. AIR Workbench is Node 22 plus a browser
   application on loopback. A converter in Rust costs nothing and runs
   everywhere; a dependency would put Node on the critical path of `muxa
   work up`.
3. **The evidence flows the other way.** AIR's trace is synthesized from
   Codex and Claude session envelopes, metadata only. muxa *observed the
   run*: it has the agent lifecycle, the stage transitions, the `work done`
   handoffs, the automation firings, and the mailbox exchanges. muxa is the
   better producer of an AIR trace than the session files are. That is the
   most valuable thing this integration can deliver, and it points from muxa
   to AIR, not the reverse.

What muxa gets in return: a graph editor it does not have to build, a
reviewable artifact for a Work run, and a plan format with an approval
semantic that matches what Start Work already does informally.

What stays muxa's: `config.toml` remains the source of truth. Every import
lands through the same validation `muxa work pipeline set` applies, so an
AIR file can never write a pipeline that would not launch.

## Plan

**Where the converter lives (decided after this plan was written).** The
integration ships in **Muxa.app only** — not in muxad, not in the `muxa` CLI
— as a module the operator switches on under Settings › Modules, with the
converter written in Swift (`Sources/Air*.swift`). Reason 2 above still
holds and is the reason for the change rather than against it: keeping AIR
out of the daemon and the CLI is exactly what keeps `muxa work up` working
on a headless host with no Node, and the app is the only surface that ever
had a graph to show. The phases below stand; read "Rust crate" as "Swift in
the app" and "CLI subcommand" as "module action". One thing did not survive
contact with the schema: AIR 1's two trace profiles each describe a *single*
`claude` or `codex` session, so a Work becomes one trace per agent — panes
running anything else are named and left out rather than relabelled — and
the fields muxa never observed (the provider's safety posture, the process
exit) are written at AIR's least-claiming values with an `info` diagnostic
in the artifact saying they are not measurements.

### Phase 0 — the converter, and one artifact that proves it

- `crates/muxa/src/air.rs`: `PipelineSpec ⇄ AIR 1 workflow`. muxa's runtime
  fields travel in an `x-muxa` extension (alias, program, role, task,
  direction, layout, `after`, prompt), so a round trip through AIR loses
  nothing and a foreign reader still sees a sensible graph.
- Vendored AIR 1 schema, validated on import; the AIR version and profile
  are pinned in one constant.
- CLI: `muxa work export <pipeline> --air`, `muxa work import <file.air.json>
  --as <name>` (import goes through `pipeline set` validation).
- **Acceptance:** `resolve` exported, opened in AIR Workbench, rendered as a
  two-stage graph; re-imported byte-identical in the fields muxa owns.

### Phase 1 — traces muxa is uniquely able to write

- `muxa work trace <work> --air`: an AIR 1 trace built from what the daemon
  already recorded — agent lifecycle, stage transitions, `work done`,
  automation firings, collaboration requests, timings.
- **Metadata only, by construction.** Prompts, agent output, file contents
  and command text never enter the exporter. This is a property of the code
  path, not a flag someone can flip.
- Muxa.app: "Export run evidence" on a finished Work.
- **Acceptance:** Workbench shows a muxa run beside a Codex/Claude session,
  with the stage graph the run actually followed.

### Phase 2 — plan and approval

- `muxa work up --dry-run --air` emits an AIR *plan*: the bound prompt bytes,
  cwd, provider, pipeline graph, and safety settings.
- `muxa work up --plan <file>` refuses to run when any bound input changed
  since the plan was approved — AIR's approval semantics, applied to muxa's
  own launch.
- Muxa.app's Start Work "Plan" step becomes that artifact, so what the GUI
  showed and what ran are the same reviewable bytes.

### Phase 3 — the editing loop

- Muxa.app's pipeline editor gains "Open in AIR Workbench": write the
  artifact, launch the loopback URL, and offer to import the result back.
- The composer (`work_compose`) can emit AIR directly, so a drafted pipeline
  is reviewable in a graph before it is saved.

### Phase 4 — Skills, if it earns its place

muxa's `[message.skills]` is a name → prompt map. AIR is built around
`SKILL.md` with lossless byte ranges. Importing a Skill into a pipeline
draft — what we did by hand for `callabo/resolve` — is the natural end of
this road, but only after Phases 0–2 prove the boundary holds.

## Decisions and risks

| Decision | Why |
|---|---|
| Converter in Rust, Workbench optional | muxa must keep working on a headless host with no Node |
| TOML stays authoritative | Routes, worktrees, prepare steps and hosts have no AIR home; losing them silently would be worse than not integrating |
| Pin the AIR version and profile; negotiate with the protocol manifest | AIR is 1.0.0 and still extracting from `agent-skills`; muxa should fail loudly on a mismatch rather than half-read an artifact |
| Traces are metadata-only in the exporter, not by option | muxa knows the prompts. The one thing that must never leak is the thing it uniquely has |
| Import always re-validates | An AIR file is untrusted input; `pipeline set`'s rules are the gate |

The risk worth stating plainly: AIR is a young private-stage format built by
the same author as muxa. Coupling two young formats can produce a single
brittle thing rather than two useful ones. The converter boundary is what
keeps that from happening — if AIR changes shape, one Rust module changes and
muxa keeps running.
