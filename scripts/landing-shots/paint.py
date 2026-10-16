#!/usr/bin/env python3
"""Paint one pane of the landing film: an agent CLI or an editor, as ANSI.

    paint.py <scene> <width> <height>  > frame.ans

Scenes are close copies of what Claude Code, Codex and a terminal editor
show in the states the film needs (working, a permission prompt, a
multiple-choice question). Like docs/demo-paint.sh they are fixtures: muxa
itself is real in the film, only the agents' screens are painted.
"""

import sys

RESET, BOLD, DIM, ITAL = "\033[0m", "\033[1m", "\033[2m", "\033[3m"


def fg(n):
    return f"\033[38;5;{n}m"


ORANGE, GREY, WHITE, GREEN, RED, BLUE, CYAN, YELLOW, MAG = (
    fg(209), fg(245), fg(255), fg(114), fg(203), fg(75), fg(80), fg(221), fg(176))


def box(width, lines, color=GREY):
    inner = width - 4
    out = [f"{color}╭{'─' * (width - 2)}╮{RESET}"]
    for text, visible in lines:
        out.append(f"{color}│{RESET} {text}{' ' * max(0, inner - visible)} {color}│{RESET}")
    out.append(f"{color}╰{'─' * (width - 2)}╯{RESET}")
    return out


def claude(prompt, tools, footer, width, gap=True):
    out = [f"{GREY}>{RESET} {WHITE}{prompt}{RESET}", ""]
    for name, arg, result in tools:
        out.append(f"{GREEN}⏺{RESET} {BOLD}{name}{RESET}({arg})")
        out.append(f"  {GREY}⎿  {result}{RESET}")
    # Prompt boxes sit right under the last tool when the pane is short.
    if gap:
        out.append("")
    return out + footer(width)


def claude_input(width):
    return box(width, [(f"{GREY}>{RESET}", 1)]) + [f"  {GREY}? for shortcuts{RESET}"]


def claude_working(label):
    def footer(width):
        return [f"{ORANGE}✻{RESET} {ORANGE}{label}…{RESET} {GREY}(38s · ↑ 1.2k tokens · esc to interrupt){RESET}", ""] + claude_input(width)
    return footer


def claude_permission(command):
    def footer(width):
        rows = [
            (f"{BOLD}Bash command{RESET}", 12),
            (f"  {command}", 2 + len(command)),
            ("", 0),
            ("Do you want to proceed?", 23),
            (f"{BLUE}❯ 1. Yes{RESET}", 8),
            ("  2. Yes, and don't ask again", 29),
            ("  3. No, and tell Claude what to do", 35),
        ]
        return box(width, rows, BLUE)
    return footer


def claude_choice(question, options):
    def footer(width):
        rows = [(f"{BOLD}☐ Retry-After{RESET}", 13), (question, len(question)), ("", 0)]
        for i, option in enumerate(options, 1):
            text = f"{'❯' if i == 1 else ' '} {i}. {option}"
            rows.append((f"{BLUE}{text}{RESET}" if i == 1 else text, len(text)))
        return box(width, rows, BLUE)
    return footer


def codex(prompt, items, footer, width):
    out = [f"{BOLD}›{RESET} {prompt}", ""]
    for head, detail in items:
        out.append(f"{WHITE}•{RESET} {BOLD}{head}{RESET}")
        if detail:
            out.append(f"  {GREY}└ {detail}{RESET}")
    out.append("")
    return out + footer(width)


def codex_working(width):
    return [f"{CYAN}◦{RESET} {BOLD}Working{RESET} {GREY}(1m 12s • esc to interrupt){RESET}", "",
            f"{BOLD}›{RESET} {GREY}Ask Codex to do anything{RESET}", f"  {GREY}⏎ send   ⌃J newline   ⌃T transcript{RESET}"]


def codex_approval(command):
    def footer(width):
        return [f"{YELLOW}Would you like to run the following command?{RESET}", "",
                f"  {GREY}${RESET} {command}", "",
                f"{CYAN}› 1. Yes, proceed{RESET}",
                "  2. Yes, and don't ask again this session",
                "  3. No, and tell Codex what to do differently"]
    return footer


EDITOR = [
    ("import", " { RateLimiter } ", "from", " \"./limit\";"),
    None,
    ("export", " const ", "orders", " = router();"),
    None,
    ("orders", ".use(", "RateLimiter", ".perKey({ rate: 60, burst: 20 }));"),
    None,
    ("orders", ".post(", "\"/v1/orders\"", ", async (req, res) => {"),
    ("  const", " order = ", "await", " createOrder(req.body);"),
    ("  res", ".status(", "201", ").json(order);"),
    ("", "});", "", ""),
]


def editor(width, height):
    out = []
    colors = (MAG, WHITE, CYAN, WHITE)
    for n, line in enumerate(EDITOR, 1):
        text = "" if line is None else "".join(f"{c}{part}" for c, part in zip(colors, line)) + RESET
        out.append(f"{GREY}{n:>3}{RESET} {text}")
    out += [f"{BLUE}~{RESET}"] * max(0, height - len(out) - 1)
    status = " NORMAL  src/routes/orders.ts "
    out.append(f"\033[48;5;110m\033[38;5;235m{BOLD}{status}{RESET}{GREY}  ts  utf-8  10:1{RESET}")
    return out


SCENES = {
    "planner-working": lambda w: claude("Plan rate limiting for /v1/orders", [
        ("Read", "docs/rate-limit.md", "Read 84 lines"),
        ("Read", "src/routes/orders.ts", "Read 212 lines")], claude_working("Planning"), w),
    "planner-choice": lambda w: claude("Plan rate limiting for /v1/orders", [
        ("Read", "docs/rate-limit.md", "Read 84 lines")], claude_choice(
            "How should 429s express Retry-After?", ["Seconds", "HTTP date", "Type something"]), w, gap=False),
    "impl-working": lambda w: codex("Implement the token bucket middleware", [
        ("Edited", "src/middleware/limit.ts (+64 -3)"), ("Ran npm test", "212 passed")], codex_working, w),
    "reviewer-working": lambda w: claude("Review the limiter diff for races", [
        ("Read", "src/middleware/limit.ts", "Read 131 lines"),
        ("Grep", "\"refill\"", "Found 6 lines")], claude_working("Reviewing"), w),
    "reviewer-permission": lambda w: claude("Review the limiter diff for races", [
        ("Read", "src/middleware/limit.ts", "Read 131 lines")], claude_permission("npm run bench -- --burst 500"), w, gap=False),
    "deploy-working": lambda w: codex("Add a canary stage with auto rollback", [
        ("Edited", "pipeline.yml (+22 -1)"), ("Ran terraform plan", "3 to add, 0 to destroy")], codex_working, w),
    "editor": lambda w: editor(w, HEIGHT),
}

if __name__ == "__main__":
    scene, width = sys.argv[1], int(sys.argv[2])
    # The pane runs `cat frame; exec cat`, so the cursor ends one line below
    # the frame: anything taller than height - 1 scrolls the top line away.
    HEIGHT = int(sys.argv[3]) - 1
    lines = SCENES[scene](width)
    if len(lines) > HEIGHT:
        sys.exit(f"paint.py: {scene} is {len(lines)} lines, pane fits {HEIGHT}")
    sys.stdout.write("\n".join(lines) + "\n")
