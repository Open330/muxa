# tend docs

| Location | Contains | Rule |
| --- | --- | --- |
| `docs/*.md` | How tend behaves today: contracts, configuration, surfaces | Must match `main`. A change in behavior updates the doc in the same PR. |
| `docs/proposals/` | Plans, designs, and reviews not yet (fully) implemented | Each file states its status at the top. When it ships, fold the result into a `docs/*.md` contract and delete or archive the proposal. |
| `docs/validation/` | Dated audits and runtime validation records | Historical evidence. Never edited to describe current behavior. |

Start with [DIRECTION.md](DIRECTION.md) for what tend is and
[ARCHITECTURE.md](ARCHITECTURE.md) for how it runs.

## Languages

English is the source of truth. Korean translations (`*.ko.md`) are kept for
user-facing guides (README, install, configuration, watch, collaboration,
Fleet, onboarding, timeline, activity, pipeline, dashboard CLI, agent
integration, architecture). A Korean file may lag; when the two disagree,
the English file wins. Internal contracts, proposals, and validation records
are written once, in either language, and not translated.
