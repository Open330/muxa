#!/usr/bin/env python3
"""Turn `tmux capture-pane -e -p` output into compact HTML for the film.

Handles SGR: reset, bold, dim, italic, underline, reverse, the 16 base
colors, 256-color and truecolor, foreground and background. Runs of the
same style share one <span>, and every line is padded to the screen width
so backgrounds (the status bar, popups) fill the row.
"""

import html
import re

# A dark terminal palette (base 16) close to common defaults.
BASE = ["#1b1d26", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#c5c8d0",
        "#5c6370", "#ff7b86", "#b5e890", "#ffd68a", "#82c4ff", "#dd9cf0", "#7fd8e2", "#ffffff"]


def xterm256(n):
    if n < 16:
        return BASE[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


SGR = re.compile(r"\x1b\[([0-9;:]*)m")
OTHER_ESC = re.compile(r"\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[()][A-Za-z0-9])")


def apply(style, params):
    codes = [int(p) if p.isdigit() else 0 for p in re.split("[;:]", params)] if params else [0]
    i = 0
    while i < len(codes):
        c = codes[i]
        if c == 0:
            style = {}
        elif c == 1:
            style["b"] = True
        elif c == 2:
            style["d"] = True
        elif c == 3:
            style["i"] = True
        elif c == 4:
            style["u"] = True
        elif c == 7:
            style["r"] = True
        elif c == 22:
            style.pop("b", None); style.pop("d", None)
        elif c == 23:
            style.pop("i", None)
        elif c == 24:
            style.pop("u", None)
        elif c == 27:
            style.pop("r", None)
        elif 30 <= c <= 37:
            style["fg"] = BASE[c - 30]
        elif 90 <= c <= 97:
            style["fg"] = BASE[c - 90 + 8]
        elif 40 <= c <= 47:
            style["bg"] = BASE[c - 40]
        elif 100 <= c <= 107:
            style["bg"] = BASE[c - 100 + 8]
        elif c == 39:
            style.pop("fg", None)
        elif c == 49:
            style.pop("bg", None)
        elif c in (38, 48) and i + 1 < len(codes):
            key = "fg" if c == 38 else "bg"
            if codes[i + 1] == 5 and i + 2 < len(codes):
                style[key] = xterm256(codes[i + 2]); i += 2
            elif codes[i + 1] == 2 and i + 4 < len(codes):
                style[key] = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5]); i += 4
        i += 1
    return style


def cells(text):
    """Escape text, pinning glyphs a monospace web font may lack (⚠, ◆, ❯…)
    to exactly one cell so the rest of the row stays on the grid. Box
    drawing and block elements are in the usual monospace fonts."""
    out = []
    for ch in text:
        if ord(ch) < 0x80 or 0x2500 <= ord(ch) <= 0x259F:
            out.append(html.escape(ch))
        else:
            out.append(f'<i class="c">{html.escape(ch)}</i>')
    return "".join(out)


def css(style):
    fg, bg = style.get("fg"), style.get("bg")
    if style.get("r"):
        fg, bg = bg or "var(--t-bg)", fg or "var(--t-fg)"
    parts = []
    if fg:
        parts.append(f"color:{fg}")
    if bg:
        parts.append(f"background:{bg}")
    if style.get("b"):
        parts.append("font-weight:700")
    if style.get("d"):
        parts.append("opacity:.6")
    if style.get("i"):
        parts.append("font-style:italic")
    if style.get("u"):
        parts.append("text-decoration:underline")
    return ";".join(parts)


def convert(text, width):
    lines_out = []
    style = {}
    for raw in text.rstrip("\n").split("\n"):
        raw = OTHER_ESC.sub(lambda m: m.group(0) if m.group(0).endswith("m") else "", raw)
        runs, visible, pos = [], 0, 0
        for m in SGR.finditer(raw):
            chunk = raw[pos:m.start()]
            if chunk:
                runs.append((css(style), chunk)); visible += len(chunk)
            style = apply(dict(style), m.group(1))
            pos = m.end()
        chunk = raw[pos:]
        if chunk:
            runs.append((css(style), chunk)); visible += len(chunk)
        if visible < width:
            runs.append((css(style), " " * (width - visible)))
        merged = []
        for s, t in runs:
            if merged and merged[-1][0] == s:
                merged[-1] = (s, merged[-1][1] + t)
            else:
                merged.append((s, t))
        lines_out.append("".join(f'<span style="{s}">{cells(t)}</span>' if s else cells(t) for s, t in merged))
    return "\n".join(lines_out)


if __name__ == "__main__":
    import sys
    print(convert(sys.stdin.read(), int(sys.argv[1])))
