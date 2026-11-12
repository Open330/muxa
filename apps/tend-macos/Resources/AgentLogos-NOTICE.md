# Agent logos

`Assets.xcassets/AgentLogos` holds the logos of the agent products Tend
runs or asks. They are third-party trademarks:

- Claude and the Claude logo, and Anthropic and the Anthropic symbol, are
  trademarks of Anthropic, PBC.
- OpenAI, Codex and the OpenAI blossom are trademarks of OpenAI.
- Gemini, Antigravity and their logos are trademarks of Google LLC.
- OpenCode and the OpenCode logo are trademarks of Anomaly.

**These marks are not covered by Tend's software licence** (MIT OR
Apache-2.0). That licence applies to Tend's code and its own artwork only.
Tend uses the marks only to identify the product the user runs. Their use
does not imply that any owner endorses, sponsors or is affiliated with Tend.

## What was done to the files

Nothing was recoloured, redrawn, cropped or put on a background. Each file
below is byte-for-byte the file its owner publishes, renamed to fit the
asset catalog. The app only scales the marks. It draws them at their
published colours and does not tint them. In the menu bar too, a logo is
drawn on its own, with no coloured disc behind it.

- **The OpenAI blossom.** OpenAI publishes it in black and in white. The
  catalog holds the black file as a template image, and Tend fills it with
  pure black on light grounds or pure white on dark grounds. These are the
  two published versions, and the app never uses any other colour.
- **Margins.** Two files have wide transparent margins: the blossom and
  the Antigravity PNG. They are scaled up inside their slot so their marks
  match the others' size. Their margins are not cut off.
- **OpenCode.** It is drawn only in a slot of at least 32×36 points. That
  slot allows its 16×20 minimum and two strokes of clear space on every
  side. It grows in 4×5 steps. In smaller slots, Tend shows the name
  "OpenCode" or a terminal symbol instead of the mark. No current slot in
  the app is that large, so OpenCode always appears as its name.
- **No mono variants.** Claude, Gemini and Antigravity publish no
  one-colour version. They appear in their colours everywhere, including
  the menu bar.

All files were fetched on 2026-11-12.

| Asset | Catalog file(s) | Exact source and variant |
|---|---|---|
| `claude` | `claude.svg` | Anthropic press kit (https://www.anthropic.com/press-kit, ZIP): `Anthropic media resources/Anthropic logos/Claude logos/3 Claude Spark/SVG/Claude Spark - Clay.svg`, the Clay spark. |
| `anthropic` | `anthropic.svg` (light), `anthropic-dark.svg` (dark) | Same press kit: `Anthropic logos/Anthropic logos/2 Anthropic symbol/SVG/Anthropic symbol - Slate.svg` and `Anthropic symbol - Ivory.svg`. |
| `openai` | `openai.svg` (template; drawn black or white) | OpenAI's `OpenAI-black-monoblossom.svg`, as published in OpenAI's own repository: https://github.com/openai/build-hours/blob/47516cc96e653b9383b38ddc4358044f281c3b8d/13-image-gen/imagegen-demo/public/OpenAI-black-monoblossom.svg. Used for Codex and for the OpenAI API engine. |
| `gemini` | `gemini.svg` | The Gemini sparkle served by gemini.google.com: https://www.gstatic.com/lamda/images/gemini_sparkle_aurora_33f86dc0c0257da337c63.svg. Used only for the Gemini Ask engine. Gemini CLI is no longer an agent kind. |
| `antigravity` | `antigravity.png` | https://antigravity.google/assets/image/antigravity-logo.png (276×276, uncropped). It is a PNG because the vector mark on the same site gets its colours from blur filters that the asset catalog's SVG renderer cannot draw. |
| `opencode` | `opencode.svg` (light), `opencode-dark.svg` (dark) | https://opencode.ai/brand/files/opencode-logo-transparent-light.svg and https://opencode.ai/brand/files/opencode-logo-transparent-dark.svg, the "Default" logo, light version for light grounds and dark version for dark ones. |

## Brand and trademark guidelines

- Anthropic: https://www.anthropic.com/legal/trademark-guidelines. "No
  alterations of our trademarks (changes to color, font, proportion, or
  otherwise) are permitted."
- OpenAI: https://openai.com/brand
- Google (Gemini, Antigravity):
  https://about.google/brand-resource-center/rules/. "Don't remove,
  distort, or alter any element of a Google Brand Feature."
- OpenCode: https://opencode.ai/brand. The minimum size, scaling steps and
  clear space above follow this page.

Anyone redistributing Tend may keep these files only under the owners'
own terms. Remove them if those terms do not allow your use.
