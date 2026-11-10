#!/usr/bin/env python3
"""Move an existing Muxa install over to Tend.

Tend reads none of Muxa's names: not its directories, environment variables,
tmux options, hook commands, MCP server name, launchd labels, or macOS app
preferences. This script finds what a Muxa install left on this machine and
renames it in place.

It only shows what it would do unless you pass --apply. Every file it edits is
copied first to `<file>.muxa-migrate-<timestamp>`. Run it once on every host
that ran Muxa, after installing Tend and before starting `tendd`.

    python3 scripts/migrate-from-muxa.py            # show the plan
    python3 scripts/migrate-from-muxa.py --apply    # do it

Standard library only; needs Python 3.8+.
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

HOME = Path.home()
MACOS = platform.system() == "Darwin"
STAMP = time.strftime("%Y%m%d-%H%M%S")
UID = os.getuid()

# --------------------------------------------------------------------------- #
# Text transforms
# --------------------------------------------------------------------------- #

FULL_PAIRS = [("MUXAD", "TENDD"), ("Muxad", "Tendd"), ("muxad", "tendd"),
              ("MUXA", "TEND"), ("Muxa", "Tend"), ("muxa", "tend")]


def full_rename(text: str) -> str:
    """Every spelling of the old name. Only for text that Muxa wrote itself."""
    for old, new in FULL_PAIRS:
        text = text.replace(old, new)
    return text


# Muxa's own directory and socket names inside paths, e.g.
# ".../Application Support/muxa/agent-integration/bootstrap.md",
# "~/.config/muxa/config.toml", "/tmp/muxa-501.sock".
PATH_FRAGMENTS = [
    (re.compile(r"(Application Support/|\.config/|\.local/share/|\.cache/|Caches/)muxa(?=/|\"|'|\s|$)"), r"\1tend"),
    (re.compile(r"\bmuxa(-\d+)?\.sock\b"), r"tend\1.sock"),
    (re.compile(r"\bmuxa(-\d+\.sock)"), r"tend\1"),
    (re.compile(r"/tmp/muxad\.(log|err)\b"), r"/tmp/tendd.\1"),
]
# The CLI and daemon as commands: a bare word not inside a path or a name.
COMMAND_WORD = re.compile(r"(?<![\w./@-])muxa(d?)(?![\w.-])")
ENV_NAME = re.compile(r"\bMUXA_([A-Z0-9_]+)")
TMUX_OPTION = re.compile(r"@muxa([_-])")


def surgical_rename(text: str) -> str:
    """For files that mix Muxa's lines with the user's own (tmux.conf, shell
    rc, CLAUDE.md): rename commands, env vars, tmux options, managed-block
    markers and Muxa's own paths, but leave other paths (a repo checked out
    at ~/code/muxa, say) alone. Managed blocks are renamed in full."""
    out, inside = [], False
    for line in text.splitlines(keepends=True):
        opens = ">>> muxa managed (" in line
        closes = "<<< muxa managed (" in line
        if opens:
            inside = True
        if inside:
            line = full_rename(line)
        else:
            for pattern, repl in PATH_FRAGMENTS:
                line = pattern.sub(repl, line)
            line = ENV_NAME.sub(r"TEND_\1", line)
            line = TMUX_OPTION.sub(r"@tend\1", line)
            line = COMMAND_WORD.sub(lambda m: "tend" + ("d" if m.group(1) else ""), line)
            line = line.replace("muxa-collaboration", "tend-collaboration")
        if closes:
            inside = False
        out.append(line)
    return "".join(out)


def json_rename_values(value, fn):
    if isinstance(value, str):
        return fn(value)
    if isinstance(value, list):
        return [json_rename_values(v, fn) for v in value]
    if isinstance(value, dict):
        return {k: json_rename_values(v, fn) for k, v in value.items()}
    return value


# --------------------------------------------------------------------------- #
# Plan / apply plumbing
# --------------------------------------------------------------------------- #

class Migration:
    def __init__(self, apply: bool, verbose: bool):
        self.apply = apply
        self.verbose = verbose
        self.done = 0
        self.notes: list[str] = []

    def step(self, what: str):
        self.done += 1
        print(("  ✓ " if self.apply else "  • ") + what)

    def note(self, what: str):
        self.notes.append(what)

    def backup(self, path: Path):
        if path.is_file() and not path.is_symlink():
            shutil.copy2(path, path.with_name(f"{path.name}.muxa-migrate-{STAMP}"))

    def rewrite(self, path: Path, transform, label: str | None = None):
        """Rewrite one text file with `transform`; skip if unchanged."""
        if not path.is_file():
            return
        try:
            old = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            return
        new = transform(old)
        if new == old:
            return
        self.step(f"rewrite {label or path}")
        if self.verbose:
            for a, b in zip(old.splitlines(), new.splitlines()):
                if a != b:
                    print(f"      - {a.strip()[:110]}\n      + {b.strip()[:110]}")
        if self.apply:
            self.backup(path)
            path.write_text(new, encoding="utf-8")

    def move(self, src: Path, dst: Path, rename_inside: bool = False):
        if not src.exists() and not src.is_symlink():
            return
        if dst.exists():
            if any(dst.iterdir()) if dst.is_dir() else True:
                self.note(f"{dst} already exists, so {src} was left in place. Merge them by hand.")
                return
            if self.apply:
                dst.rmdir()
        self.step(f"move {src} → {dst}")
        if self.apply:
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(str(src), str(dst))

    def run(self, cmd: list[str], label: str, check: bool = False):
        self.step(label)
        if self.apply:
            subprocess.run(cmd, check=check, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def capture(cmd: list[str]) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.TimeoutExpired):
        return ""


def have(cmd: str) -> bool:
    return shutil.which(cmd) is not None


def xdg(var: str, default: Path) -> Path:
    value = os.environ.get(var)
    return Path(value) if value and os.path.isabs(value) else default


# --------------------------------------------------------------------------- #
# Steps
# --------------------------------------------------------------------------- #

def stop_services(m: Migration):
    print("\nServices")
    if MACOS:
        agents = HOME / "Library/LaunchAgents"
        for label in ("dev.open330.muxad", "dev.open330.muxa-restore"):
            plist = agents / f"{label}.plist"
            loaded = label in capture(["launchctl", "list"])
            if loaded:
                m.run(["launchctl", "bootout", f"gui/{UID}/{label}"], f"stop launchd job {label}")
            if plist.exists():
                m.step(f"remove {plist} (tend init installs dev.open330.tendd)")
                if m.apply:
                    m.backup(plist)
                    plist.unlink()
    if have("systemctl"):
        for unit in ("muxad.service", "muxa-restore.service"):
            if capture(["systemctl", "--user", "list-unit-files", unit]).count(unit):
                m.run(["systemctl", "--user", "disable", "--now", unit], f"disable systemd unit {unit}")
        for base in (xdg("XDG_DATA_HOME", HOME / ".local/share"), xdg("XDG_CONFIG_HOME", HOME / ".config")):
            for unit in ("muxad.service", "muxa-restore.service"):
                path = base / "systemd/user" / unit
                if path.exists():
                    m.step(f"remove {path}")
                    if m.apply:
                        m.backup(path)
                        path.unlink()
    if have("brew") and "muxa" in capture(["brew", "services", "list"]).split():
        m.run(["brew", "services", "stop", "muxa"], "stop brew service muxa")
    if capture(["pgrep", "-x", "muxad"]).strip():
        m.run(["pkill", "-x", "muxad"], "stop the running muxad")


def move_directories(m: Migration) -> list[Path]:
    """Returns Tend's directories after the move (for later rewrites)."""
    print("\nDirectories")
    if MACOS:
        support = HOME / "Library/Application Support"
        pairs = [(xdg("XDG_DATA_HOME", support) / "muxa", xdg("XDG_DATA_HOME", support) / "tend"),
                 (HOME / "Library/Caches/muxa", HOME / "Library/Caches/tend")]
        cfg = xdg("XDG_CONFIG_HOME", support)
        if cfg != xdg("XDG_DATA_HOME", support):
            pairs.append((cfg / "muxa", cfg / "tend"))
    else:
        pairs = [(xdg("XDG_DATA_HOME", HOME / ".local/share") / "muxa", xdg("XDG_DATA_HOME", HOME / ".local/share") / "tend"),
                 (xdg("XDG_CONFIG_HOME", HOME / ".config") / "muxa", xdg("XDG_CONFIG_HOME", HOME / ".config") / "tend"),
                 (xdg("XDG_CACHE_HOME", HOME / ".cache") / "muxa", xdg("XDG_CACHE_HOME", HOME / ".cache") / "tend")]
    for src, dst in pairs:
        m.move(src, dst)
    return [dst for _, dst in pairs]


def rewrite_own_files(m: Migration, dirs: list[Path]):
    """Files Muxa wrote inside its own directories: rename everything."""
    print("\nTend's own files")
    for base in dirs:
        # When only planning, the directory has not moved yet: look at the source.
        look = base if base.exists() else base.with_name("muxa")
        if not look.exists():
            continue
        bundle = look / "agent-integration"
        old_skill = bundle / "skills/muxa-collaboration"
        if old_skill.exists():
            m.step(f"rename {old_skill.name} → tend-collaboration in {bundle}")
            if m.apply:
                old_skill.rename(bundle / "skills/tend-collaboration")
        for name in ("config.toml", "agent-integration/manifest.json", "agent-integration/bootstrap.md"):
            m.rewrite(look / name, full_rename)
        for path in sorted((bundle / "skills").rglob("*.md")) if (bundle / "skills").exists() else []:
            m.rewrite(path, full_rename)
        agents = look / "agents"
        if agents.exists():
            for path in agents.glob("*.toml"):
                m.rewrite(path, surgical_rename)


def claude_homes(extra: list[str]) -> list[Path]:
    homes = [HOME / ".claude"]
    if os.environ.get("CLAUDE_CONFIG_DIR"):
        homes.append(Path(os.environ["CLAUDE_CONFIG_DIR"]))
    homes += [Path(p).expanduser() for p in extra]
    seen, out = set(), []
    for h in homes:
        if h.resolve() not in seen and h.exists():
            seen.add(h.resolve())
            out.append(h)
    return out


def rewrite_json(m: Migration, path: Path, transform):
    if not path.is_file():
        return
    try:
        old = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError):
        m.note(f"{path} is not valid JSON; check it by hand for muxa entries.")
        return
    new = transform(json.loads(json.dumps(old)))
    if new == old:
        return
    m.step(f"rewrite {path}")
    if m.apply:
        m.backup(path)
        path.write_text(json.dumps(new, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def hook_command(cmd: str) -> str:
    cmd = re.sub(r"(?<![\w./-])muxa( hook\b)", r"tend\1", cmd)
    return surgical_rename(cmd)


def fix_hooks(node):
    """Rename `muxa hook …` commands anywhere in a hooks structure."""
    return json_rename_values(node, lambda s: hook_command(s) if "muxa" in s else s)


def fix_mcp_servers(servers: dict) -> dict:
    if not isinstance(servers, dict) or "muxa" not in servers:
        return servers
    entry = json_rename_values(servers.pop("muxa"), surgical_rename)
    if isinstance(entry, dict) and isinstance(entry.get("env"), dict):
        entry["env"] = {ENV_NAME.sub(r"TEND_\1", k): v for k, v in entry["env"].items()}
    servers.setdefault("tend", entry)
    return servers


def migrate_agents(m: Migration, extra_claude: list[str]):
    print("\nAgent integrations")
    for home in claude_homes(extra_claude):
        rewrite_json(m, home / "settings.json", lambda d: {**d, **({"hooks": fix_hooks(d["hooks"])} if "hooks" in d else {}),
                                                         **({"statusLine": fix_hooks(d["statusLine"])} if "statusLine" in d else {})})
        m.rewrite(home / "CLAUDE.md", surgical_rename)
        migrate_skill_link(m, home / "skills")
    for claude_json in {HOME / ".claude.json", *[h / ".claude.json" for h in claude_homes(extra_claude)]}:
        def fix(d):
            if "mcpServers" in d:
                d["mcpServers"] = fix_mcp_servers(d["mcpServers"])
            for proj in (d.get("projects") or {}).values():
                if isinstance(proj, dict) and "mcpServers" in proj:
                    proj["mcpServers"] = fix_mcp_servers(proj["mcpServers"])
            return d
        rewrite_json(m, claude_json, fix)

    codex = Path(os.environ.get("CODEX_HOME", HOME / ".codex"))
    def fix_codex_toml(text: str) -> str:
        text = re.sub(r"\[mcp_servers\.muxa(\]|\.)", r"[mcp_servers.tend\1", text)
        return surgical_rename(text)
    m.rewrite(codex / "config.toml", fix_codex_toml)
    for name in ("AGENTS.md", "AGENTS.override.md"):
        m.rewrite(codex / name, surgical_rename)
    migrate_skill_link(m, HOME / ".agents/skills")

    rewrite_json(m, HOME / ".gemini/settings.json", lambda d: {**d, **({"hooks": fix_hooks(d["hooks"])} if "hooks" in d else {})})
    def fix_agy(d):
        if isinstance(d, dict) and "muxa" in d:
            d.setdefault("tend", fix_hooks(d.pop("muxa")))
        return d
    rewrite_json(m, HOME / ".gemini/config/hooks.json", fix_agy)

    opencode = (HOME / "Library/Application Support" if MACOS else xdg("XDG_CONFIG_HOME", HOME / ".config")) / "opencode/plugins"
    old_plugin = opencode / "muxa.ts"
    if old_plugin.exists():
        m.step(f"move {old_plugin} → tend.ts")
        if m.apply:
            m.backup(old_plugin)
            new_plugin = opencode / "tend.ts"
            new_plugin.write_text(full_rename(old_plugin.read_text(encoding="utf-8")), encoding="utf-8")
            old_plugin.unlink()


def migrate_skill_link(m: Migration, skills_dir: Path):
    old = skills_dir / "muxa-collaboration"
    if not old.is_symlink() and not old.exists():
        return
    target = os.readlink(old) if old.is_symlink() else None
    new = skills_dir / "tend-collaboration"
    if target:
        new_target = surgical_rename(target).replace("muxa-collaboration", "tend-collaboration")
        m.step(f"relink {new} → {new_target}")
        if m.apply:
            old.unlink()
            if not new.exists() and not new.is_symlink():
                new.symlink_to(new_target)
    else:
        m.note(f"{old} is a real directory, not Muxa's symlink; left in place.")


def migrate_shell_and_tmux(m: Migration):
    print("\nShell and tmux configuration")
    for rc in (HOME / ".zshrc", HOME / ".bashrc", HOME / ".bash_profile", HOME / ".profile",
               HOME / ".config/fish/config.fish", HOME / ".zprofile", HOME / ".zshenv"):
        m.rewrite(rc, surgical_rename)
    for conf in (HOME / ".tmux.conf", xdg("XDG_CONFIG_HOME", HOME / ".config") / "tmux/tmux.conf", HOME / ".rmux.conf"):
        m.rewrite(conf, surgical_rename)


def tmux_servers() -> list[list[str]]:
    servers = [["tmux"]]
    for var in ("MUXA_TMUX_SOCKET", "TEND_TMUX_SOCKET"):
        if os.environ.get(var):
            servers.append(["tmux", "-S", os.environ[var]])
    return servers


def migrate_live_tmux(m: Migration):
    """Copy @muxa_* user options on live sessions/windows/panes to @tend_*,
    so running Work keeps its identity, then unset the old ones."""
    print("\nLive tmux options")
    if not have("tmux"):
        return
    for tmux in tmux_servers():
        if not capture(tmux + ["list-sessions", "-F", "#{session_id}"]).strip():
            continue
        sessions = capture(tmux + ["list-sessions", "-F", "#{session_id}"]).split()
        windows = capture(tmux + ["list-windows", "-a", "-F", "#{window_id}"]).split()
        panes = capture(tmux + ["list-panes", "-a", "-F", "#{pane_id}"]).split()
        targets = [("", t) for t in sessions] + [("-w", t) for t in windows] + [("-p", t) for t in panes]
        count = 0
        for scope, target in targets:
            args = [a for a in (scope,) if a]
            listing = capture(tmux + ["show-options"] + args + ["-t", target])
            for line in listing.splitlines():
                name = line.split(" ", 1)[0]
                if not (name.startswith("@muxa_") or name.startswith("@muxa-")):
                    continue
                value = capture(tmux + ["show-options"] + args + ["-qv", "-t", target, name]).rstrip("\n")
                new_name = TMUX_OPTION.sub(r"@tend\1", name)
                count += 1
                if m.apply:
                    subprocess.run(tmux + ["set-option"] + args + ["-t", target, new_name, value], capture_output=True)
                    subprocess.run(tmux + ["set-option"] + args + ["-u", "-t", target, name], capture_output=True)
        env = capture(tmux + ["show-environment", "-g"])
        sock = re.search(r"^MUXA_SOCKET=(.*)$", env, re.M)
        if count:
            m.step(f"{' '.join(tmux)}: rename {count} @muxa_* options to @tend_*")
        if sock:
            m.step(f"{' '.join(tmux)}: TEND_SOCKET={surgical_rename(sock.group(1))} in the global environment")
            if m.apply:
                subprocess.run(tmux + ["set-environment", "-g", "TEND_SOCKET", surgical_rename(sock.group(1))], capture_output=True)
                subprocess.run(tmux + ["set-environment", "-gu", "MUXA_SOCKET"], capture_output=True)


PREF_KEY_RENAMES = [(re.compile(r"^muxa\."), "tend."), (re.compile(r"^Muxa"), "Tend")]


def migrate_macos_app(m: Migration):
    if not MACOS:
        return
    print("\nmacOS app")
    raw = subprocess.run(["defaults", "export", "dev.muxa.mac", "-"], capture_output=True)
    if raw.returncode == 0 and raw.stdout:
        old = plistlib.loads(raw.stdout)
        current = subprocess.run(["defaults", "export", "dev.tend.mac", "-"], capture_output=True)
        merged = plistlib.loads(current.stdout) if current.returncode == 0 and current.stdout else {}
        added = 0
        for key, value in old.items():
            new_key = key
            for pattern, repl in PREF_KEY_RENAMES:
                new_key = pattern.sub(repl, new_key)
            if new_key not in merged:
                merged[new_key] = value
                added += 1
        if added:
            m.step(f"copy {added} preferences from dev.muxa.mac to dev.tend.mac")
            if m.apply:
                tmp = Path(os.environ.get("TMPDIR", "/tmp")) / f"tend-prefs-{os.getpid()}.plist"
                tmp.write_bytes(plistlib.dumps(merged))
                subprocess.run(["defaults", "import", "dev.tend.mac", str(tmp)], check=True)
                tmp.unlink()
    migrate_keychain(m)
    for app in (Path("/Applications/Muxa.app"), HOME / "Applications/Muxa.app"):
        if app.exists():
            m.note(f"{app} is still installed. Quit it and move it to the Trash once Tend.app works.")


def migrate_keychain(m: Migration):
    """Ask provider API keys: copy dev.muxa.mac.ask-provider items to
    dev.tend.mac.ask-provider. macOS asks once per key to allow the read."""
    dump = capture(["security", "dump-keychain"])
    accounts, current = set(), {}
    for line in dump.splitlines():
        if line.startswith("keychain:"):
            if current.get("svce") == "dev.muxa.mac.ask-provider" and current.get("acct"):
                accounts.add(current["acct"])
            current = {}
        match = re.search(r'"(acct|svce)"<blob>="([^"]*)"', line)
        if match:
            current[match.group(1)] = match.group(2)
    if current.get("svce") == "dev.muxa.mac.ask-provider" and current.get("acct"):
        accounts.add(current["acct"])
    for account in sorted(accounts):
        m.step(f"copy Ask API key {account} to the dev.tend.mac.ask-provider keychain service")
        if m.apply:
            secret = subprocess.run(["security", "find-generic-password", "-s", "dev.muxa.mac.ask-provider",
                                     "-a", account, "-w"], capture_output=True, text=True)
            if secret.returncode != 0:
                m.note(f"Could not read the {account} key; enter it again in Tend › Settings › Ask.")
                continue
            subprocess.run(["security", "add-generic-password", "-U", "-s", "dev.tend.mac.ask-provider",
                            "-a", account, "-w", secret.stdout.rstrip("\n")], capture_output=True)


def report_leftovers(m: Migration):
    print("\nBinaries")
    for name in ("muxa", "muxad"):
        path = shutil.which(name)
        if path:
            m.note(f"{path} is still on PATH. Remove it once Tend works "
                   f"({'brew uninstall muxa' if '/Cellar/' in os.path.realpath(path) or 'homebrew' in path else 'cargo uninstall muxa-cli muxad, or delete the file'}).")
    if not shutil.which("tend"):
        m.note("`tend` is not on PATH yet. Install it first: brew install open330/tap/tend")
    env = sorted(k for k in os.environ if k.startswith("MUXA_"))
    if env:
        m.note("This shell still exports " + ", ".join(env) + ". Open a new shell after migrating.")


def main() -> int:
    parser = argparse.ArgumentParser(description="Move an existing Muxa install over to Tend.")
    parser.add_argument("--apply", action="store_true", help="make the changes (default: only show them)")
    parser.add_argument("--verbose", "-v", action="store_true", help="show each changed line")
    parser.add_argument("--claude-home", action="append", default=[], metavar="DIR",
                        help="another Claude Code config dir to migrate (repeatable)")
    parser.add_argument("--skip-tmux", action="store_true", help="leave live tmux sessions alone")
    parser.add_argument("--only-files", action="store_true",
                        help="only move directories and rewrite files; leave services, live tmux, "
                             "macOS preferences and the keychain alone")
    args = parser.parse_args()

    m = Migration(apply=args.apply, verbose=args.verbose)
    print("Migrating Muxa → Tend" + ("" if args.apply else " (plan only; pass --apply to make these changes)"))
    if not args.only_files:
        stop_services(m)
    dirs = move_directories(m)
    rewrite_own_files(m, dirs)
    migrate_agents(m, args.claude_home)
    migrate_shell_and_tmux(m)
    if not (args.skip_tmux or args.only_files):
        migrate_live_tmux(m)
    if not args.only_files:
        migrate_macos_app(m)
    report_leftovers(m)

    print()
    if m.done == 0:
        print("Nothing from Muxa found on this machine.")
    elif args.apply:
        print(f"Done: {m.done} changes. Backups end in .muxa-migrate-{STAMP}.")
        print("Next: run `tend init` to install the tendd service, then restart your agents so their hooks call `tend`.")
    else:
        print(f"{m.done} changes planned. Run again with --apply to make them.")
    if m.notes:
        print("\nCheck by hand:")
        for note in m.notes:
            print("  - " + note)
    return 0


if __name__ == "__main__":
    sys.exit(main())
