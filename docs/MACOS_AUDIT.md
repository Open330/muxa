# macOS workbench regression audit

## Scope

Reviewed the workbench's search ranking, destination identity, query and selection
updates, native search-field input, command dispatch, editor tab/group lifecycle,
prompt send guards, draft preservation, configuration write/conflict handling,
and Ask automation judgment boundaries. Ran the complete macOS test target and
the muxa, muxad, and muxa-cli Rust test suites, plus Rust Clippy.

This is a regression review of these paths, not a claim that every application
state, platform combination, or production integration is free of bugs.

## Corrected findings

| Finding | Correction | Regression coverage |
| --- | --- | --- |
| Closing an editor in a background split group changed the focused group. | Preserve the focused group unless that group itself is removed. | Close both a background tab and its final tab without moving focus. |
| Dismissing an exited shell could remove the focused group without synchronizing the model's active editor. | Close the shell everywhere, then synchronize once to the surviving editor. | Remove the same shell from both groups and check the surviving editor and empty-workbench case. |

Previously corrected palette defects remain covered: editable native search,
Command-P/Command-Shift-P dispatch, exact pane-title precedence, and selecting the
new best match when the query changes rather than retaining a weaker old result.

## Expanded verification

- Native full-palette view: type a partial ticket ID, extend it to an exact ID,
  press Enter, and verify the chosen pane identity rather than just its score.
- Native full-palette view: move to a different result with Tab, refresh the
  execution snapshot, then verify Enter still opens the manually selected item.
- Editor state: 1,200 deterministic mixed operations covering previews, pinned
  tabs, splits, closing, pruning, and keyboard cycling. Check group count, focused
  group membership, unique tabs/history, and active/preview membership after
  every operation.
- Existing coverage includes cross-host/socket identities, duplicate names,
  disabled commands, IME key routing, fixed-size prompt feedback, in-flight draft
  edits, config conflicts and round trips, and automation safety/budget rules.

Validation: 200 Swift tests pass; 2,105 Rust tests pass with one ignored;
Clippy with warnings denied passes. Production signing and bundle smoke tests
are checked separately during packaging.

## Boundaries

No real prompts, paid Ask requests, or user configuration writes are used for
this audit. The running daemon and its sessions are retained during app updates.
Live remote failures, every keyboard layout/input method, and every possible
multi-window workflow still require dedicated manual or integration testing.
