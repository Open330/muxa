# Tend mark — state rule A (a fixed state per room)

The four rooms of the F2 mark each stand for one kind of agent state.
A room lights when at least one agent is in that state; otherwise it is dimmed.

| Room | Position | State | Lit colour |
| --- | --- | --- | --- |
| `room-waiting` | top right (small) | waiting on you (input, choice) | amber `#FFAA2B` |
| `room-error` | top left | error or blocked | coral `#FF5A3C` |
| `room-working` | bottom right (large) | working | ink `#16181D` on light, paper `#F2F3F5` on dark |
| `room-done` | bottom left | finished | green `#3FBF84` |

Dimmed rooms keep the theme ink/paper colour at 22% (light) or 26% (dark) opacity.
With no agents at all every room is dimmed.

## Files

- `assets/brand/tend/mark-rooms.svg`: master with `id`/`class` per room and `fill="currentColor"`; colour the rooms in code.
- `assets/brand/tend/states/menubar-WERD-THEME[-16|-32].{svg,png}`: all 16 combinations, pre-rendered.
  The four digits are waiting, error, working, done (1 = lit). Example: `menubar-1010-dark-32.png`
  means "someone is waiting and others are working" on a dark menu bar.

On macOS, the calm state (no waiting, no error) can use `menubar-template.svg` as a template image so
the system tints it; any state that shows colour must use a non-template image from `states/`.

## App icon

The app icon (`assets/icon.svg`, copied to `appicon-states.svg`) shows all four rooms lit in their
state colours on the graphite ground, so the icon itself teaches the rule: coral top left, amber top
right, green bottom left, paper bottom right. The macOS app ships it as an Icon Composer
bundle, `apps/tend-macos/Resources/AppIcon.icon`: one SVG layer per room over an automatic graphite
gradient, so macOS 26 and later draw it at full size in the system squircle instead of shrinking a
pre-rounded image into a grey plate. Edit it in Xcode's Icon Composer, or change the room SVGs and
`icon.json` by hand; keep `assets/icon.svg` in step. `appicon-calm.svg` and `appicon-call.svg` are the earlier white and
single-amber versions, kept for reference.
