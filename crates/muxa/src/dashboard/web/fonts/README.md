# Bundled fonts

## `muxa-nerd-symbols.woff2`

Pane output (powerlevel10k/starship prompts, Claude Code and Codex status
lines) carries Nerd Font glyphs from the Unicode Private Use Areas. Without a
font that covers them the dashboard renders empty boxes, so the dashboard
ships a symbols-only fallback. CSS loads it with a `unicode-range` limited to
the PUA, so browsers fetch it only when a page actually shows such a glyph and
normal text keeps the primary monospace font.

- Upstream: [ryanoasis/nerd-fonts](https://github.com/ryanoasis/nerd-fonts),
  release **v3.5.1**
- Source asset:
  <https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/NerdFontsSymbolsOnly.tar.xz>
  (SHA-256 `01172f37db8543edb102e5cb5c64101c9f4686630804d49b419aa07b23a69996`)
- Source file: `SymbolsNerdFontMono-Regular.ttf` ("Symbols Nerd Font Mono")
- Committed file SHA-256:
  `4bffba5331d5adc7530afd22b8d7592ddcce89d479d82269dc1ad4fed4f4060e`
- License: MIT, see [`LICENSE-nerd-fonts-symbols.txt`](LICENSE-nerd-fonts-symbols.txt).
  The icon sets inside keep their upstream licenses, as listed in the release's
  README: Codicons and Font Awesome (CC BY 4.0), Material Design (Apache 2.0),
  Pomicons and Weather Icons (SIL OFL 1.1), Devicons, Octicons, Seti UI,
  Powerline symbols, Font Awesome Extension, Power Symbols IEC and the Hack
  extra glyphs (MIT), Font Logos (unlicensed).

Regenerate it with fonttools 4.66.0 (in a virtualenv):

```sh
python3 -m venv .venv && .venv/bin/pip install fonttools brotli
curl -fsSLO https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/NerdFontsSymbolsOnly.tar.xz
tar -xJf NerdFontsSymbolsOnly.tar.xz SymbolsNerdFontMono-Regular.ttf
.venv/bin/pyftsubset SymbolsNerdFontMono-Regular.ttf \
  --unicodes="U+E000-F8FF,U+F0000-FFFFF" --flavor=woff2 \
  --layout-features='' --no-hinting --desubroutinize --name-IDs='*' \
  --notdef-outline --output-file=muxa-nerd-symbols.woff2
```

The subset keeps only the two Private Use Areas Nerd Fonts assigns glyphs in
(U+E000–U+F8FF and U+F0000–U+FFFFF). Box drawing (U+2500–U+259F) is left to the
primary monospace font, which already covers it.
