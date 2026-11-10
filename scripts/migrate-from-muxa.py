#!/usr/bin/env python3
"""Move an existing Muxa install over to Tend.

Tend reads none of Muxa's names: not its directories, environment variables,
tmux options, hook commands, MCP server name, launchd labels, or macOS app
preferences. This script finds what a Muxa install left on this machine and
renames it in place.

It only shows what it would do unless you pass --apply. Every file it edits is
copied first to `<file>.muxa-migrate-<timestamp>` (for a symlinked dotfile,
the file the link points to). Run it once on every host that ran Muxa, after
installing Tend and before starting `tendd`.

    python3 scripts/migrate-from-muxa.py            # show the plan
    python3 scripts/migrate-from-muxa.py --apply    # do it

Standard library only; needs Python 3.8+. TOML files (Tend's config.toml,
Codex's config.toml) are only edited under Python 3.11+, which can check the
result; with an older Python they are reported for a manual edit.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import os
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

try:  # Python 3.11+
    import tomllib
except ImportError:  # pragma: no cover
    tomllib = None

HOME = Path.home()
MACOS = platform.system() == "Darwin"
STAMP = time.strftime("%Y%m%d-%H%M%S")
UID = os.getuid()

OLD_SERVICE = "dev.muxa.mac.ask-provider"
NEW_SERVICE = "dev.tend.mac.ask-provider"

# --------------------------------------------------------------------------- #
# Text transforms
# --------------------------------------------------------------------------- #

FULL_PAIRS = [("MUXAD", "TENDD"), ("Muxad", "Tendd"), ("muxad", "tendd"),
              ("MUXA", "TEND"), ("Muxa", "Tend"), ("muxa", "tend")]


def full_rename(text: str) -> str:
    """Every spelling of the old name. Only for documents Muxa authored
    entirely (its bootstrap and skill files, its managed blocks), never for
    user values."""
    for old, new in FULL_PAIRS:
        text = text.replace(old, new)
    return text


# Directories this run moves, filled in by move_directories(): every path
# that pointed into one of them is rewritten to the new location, wherever
# XDG put it.
MOVES: list[tuple[str, str]] = []

# Muxa's own directory and socket names at their default locations, for
# paths written on another machine or before an XDG change.
PATH_FRAGMENTS = [
    (re.compile(r"(Application Support/|\.config/|\.local/share/|\.cache/|Caches/)muxa(?=/|\"|'|\s|$)"), r"\1tend"),
    (re.compile(r"\bmuxa(-\d+)?\.sock\b"), r"tend\1.sock"),
    (re.compile(r"/tmp/muxad\.(log|err)\b"), r"/tmp/tendd.\1"),
]
# The CLI and daemon as a bare command word, not part of a path or a name.
COMMAND_WORD = re.compile(r"(?<![\w./@-])muxa(d?)(?![\w.-])")
ENV_NAME = re.compile(r"\bMUXA_([A-Z0-9_]+)")
TMUX_OPTION = re.compile(r"@muxa([_-])")


def moved_paths(text: str) -> str:
    for src, dst in MOVES:
        variants = [(src, dst)]
        if src.startswith(str(HOME) + "/"):
            rel_s, rel_d = src[len(str(HOME)):], dst[len(str(HOME)):]
            variants += [("~" + rel_s, "~" + rel_d), ("$HOME" + rel_s, "$HOME" + rel_d)]
        for s, d in variants:
            text = re.sub(re.escape(s) + r"(?=/|\"|'|\s|$)", lambda _m, d=d: d, text, flags=re.M)
    return text


def rename_executable(command: str) -> str:
    """`muxa …`, `/opt/bin/muxa …` or `…/muxad` as the program of a command
    line: rename the program, keep its directory and the arguments."""
    match = re.match(r"\s*[\"']?(\S*?)[\"']?(?:\s|$)", command)
    if not match:
        return command
    program = match.group(1)
    head, slash, base = program.rpartition("/")
    if base not in ("muxa", "muxad"):
        return command
    renamed = head + slash + ("tend" if base == "muxa" else "tendd")
    return command[:match.start(1)] + renamed + command[match.end(1):]


def rename_program_path(path: str) -> str:
    """An MCP `command` or a fleet `tend_path` is the executable itself, spaces
    and all: rename only its file name."""
    head, slash, base = path.rpartition("/")
    if base in ("muxa", "muxad"):
        return head + slash + ("tend" if base == "muxa" else "tendd")
    return path


def config_toml(text: str) -> str:
    """Tend's config.toml: the fleet key `muxa_path` is now `tend_path` (its
    value renamed by file name), then the surgical rename. Other values,
    such as a cwd under ~/code/muxa, keep their spelling."""
    text = re.sub(r"\bmuxa_path(\s*=\s*)([\"'])(.*?)\2",
                  lambda m: f"tend_path{m.group(1)}{m.group(2)}{rename_program_path(m.group(3))}{m.group(2)}", text)
    return surgical_rename(text)


def surgical_rename(text: str) -> str:
    """For files that mix Muxa's entries with the user's own: rename commands,
    env vars, tmux options and Muxa's own paths, but leave other values (a
    repo checked out at ~/code/muxa, a cwd, a URL) alone. Muxa's managed
    blocks are its own text and are renamed in full."""
    out, inside = [], False
    for line in text.splitlines(keepends=True):
        if ">>> muxa managed (" in line:
            inside = True
        if inside:
            # Muxa's own text, except the home directory in its absolute
            # paths: a home or parent folder named like "muxa" keeps its name.
            line = moved_paths(line).replace(str(HOME), "\0HOME\0")
            line = full_rename(line).replace("\0HOME\0", str(HOME))
        else:
            line = moved_paths(line)
            for pattern, repl in PATH_FRAGMENTS:
                line = pattern.sub(repl, line)
            line = ENV_NAME.sub(r"TEND_\1", line)
            line = TMUX_OPTION.sub(r"@tend\1", line)
            line = COMMAND_WORD.sub(lambda m: "tend" + ("d" if m.group(1) else ""), line)
            line = line.replace("muxa-collaboration", "tend-collaboration")
        if inside and "<<< tend managed (" in line:
            inside = False
        out.append(line)
    return "".join(out)


def json_strings(value, fn):
    if isinstance(value, str):
        return fn(value)
    if isinstance(value, list):
        return [json_strings(v, fn) for v in value]
    if isinstance(value, dict):
        return {k: json_strings(v, fn) for k, v in value.items()}
    return value


def fix_commands(node):
    """Hook structures: the `command` of an entry is a command line; any
    other string only has Muxa's paths and names renamed."""
    if isinstance(node, list):
        return [fix_commands(v) for v in node]
    if isinstance(node, dict):
        return {k: (surgical_rename(rename_executable(v)) if k == "command" and isinstance(v, str)
                    else fix_commands(v)) for k, v in node.items()}
    if isinstance(node, str) and ("muxa" in node or "MUXA" in node):
        return surgical_rename(node)
    return node


# --------------------------------------------------------------------------- #
# Safe writes
# --------------------------------------------------------------------------- #

def real(path: Path) -> Path:
    """Edit what a dotfile symlink points to, keeping the link."""
    return Path(os.path.realpath(path)) if path.is_symlink() else path


@contextlib.contextmanager
def owner_lock(path: Path):
    """Claude Code guards `.claude.json` with proper-lockfile: a `<file>.lock`
    directory, stale after 10 s. Take the same lock while editing it, as
    `tend init` does."""
    if path.name != ".claude.json" or not path.parent.is_dir():
        yield
        return
    lock = path.with_name(".claude.json.lock")
    deadline = time.time() + 5
    while True:
        try:
            lock.mkdir()
            break
        except FileExistsError:
            with contextlib.suppress(OSError):
                if time.time() - lock.stat().st_mtime > 10:
                    lock.rmdir()
                    continue
            if time.time() > deadline:
                raise RuntimeError(f"{path} is locked by Claude Code; run the script again")
            time.sleep(0.1)
    try:
        yield
    finally:
        with contextlib.suppress(OSError):
            lock.rmdir()


def atomic_write(path: Path, text: str):
    mode = path.stat().st_mode & 0o7777 if path.exists() else 0o600
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.tend-migrate-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(text)
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp)
        raise


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

    def edit(self, path: Path, transform, validate=None):
        """Read, transform and write one text file safely: the symlink
        target, Claude Code's lock, a check that nobody wrote it meanwhile,
        a backup, and an atomic replace that keeps the file mode."""
        target = real(path)
        if not target.is_file():
            return
        try:
            with owner_lock(target):
                old = target.read_text(encoding="utf-8")
                new = transform(old)
                if new == old:
                    return
                noted = len(self.notes)
                if validate and not validate(new):
                    if len(self.notes) > noted:
                        pass  # the validator already explained why
                    elif tomllib is None:
                        self.note(f"{path} needs renaming, but editing TOML safely needs Python 3.11+. "
                                  f"Run the script with python3.11 (or newer), or edit it by hand.")
                    else:
                        self.note(f"{path}: the renamed file would not parse, so it was left unchanged. Edit it by hand.")
                    return
                self.step(f"rewrite {path}" + (f" (→ {target})" if target != path else ""))
                if self.verbose:
                    for a, b in zip(old.splitlines(), new.splitlines()):
                        if a != b:
                            print(f"      - {a.strip()[:110]}\n      + {b.strip()[:110]}")
                if not self.apply:
                    return
                if target.read_text(encoding="utf-8") != old:
                    self.note(f"{path} changed while migrating; run the script again.")
                    return
                shutil.copy2(target, target.with_name(f"{target.name}.muxa-migrate-{STAMP}"))
                atomic_write(target, new)
        except (UnicodeDecodeError, PermissionError):
            return
        except RuntimeError as err:
            self.note(str(err))

    def edit_json(self, path: Path, transform):
        def text_transform(text: str) -> str:
            try:
                data = json.loads(text)
            except json.JSONDecodeError:
                self.note(f"{path} is not valid JSON; check it by hand for muxa entries.")
                return text
            new = transform(json.loads(text))
            return text if new == data else json.dumps(new, indent=2, ensure_ascii=False) + "\n"
        self.edit(path, text_transform)

    def move(self, src: Path, dst: Path):
        if not src.exists() and not src.is_symlink():
            return
        if dst.exists() and (not dst.is_dir() or any(dst.iterdir())):
            self.note(f"{dst} already exists, so {src} was left in place. Merge them by hand.")
            return
        MOVES.append((str(src), str(dst)))
        self.step(f"move {src} → {dst}")
        if self.apply:
            if dst.exists():
                dst.rmdir()
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(str(src), str(dst))

    def run(self, cmd: list[str], label: str):
        self.step(label)
        if self.apply:
            subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def remove(self, path: Path, label: str):
        self.step(label)
        if self.apply:
            shutil.copy2(path, path.with_name(f"{path.name}.muxa-migrate-{STAMP}"))
            path.unlink()


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


def valid_toml(text: str) -> bool:
    """TOML edits are written only when they can be checked (Python 3.11+)."""
    if tomllib is None:
        return False
    try:
        tomllib.loads(text)
        return True
    except tomllib.TOMLDecodeError:
        return False


# --------------------------------------------------------------------------- #
# Steps
# --------------------------------------------------------------------------- #

def stop_services(m: Migration):
    print("\nServices")
    if MACOS:
        agents = HOME / "Library/LaunchAgents"
        loaded = capture(["launchctl", "list"])
        for label in ("dev.open330.muxad", "dev.open330.muxa-restore"):
            if label in loaded:
                m.run(["launchctl", "bootout", f"gui/{UID}/{label}"], f"stop launchd job {label}")

    if have("systemctl"):
        for unit in ("muxad.service", "muxa-restore.service"):
            if unit in capture(["systemctl", "--user", "list-unit-files", unit]):
                m.run(["systemctl", "--user", "disable", "--now", unit], f"disable systemd unit {unit}")

    # `brew services list` lists every formula with a service block; only
    # one whose status is not "none"/"stopped" actually runs as a service.
    brew_status = re.search(r"^muxa\s+(\S+)", capture(["brew", "services", "list"]) if have("brew") else "", re.M)
    if brew_status and brew_status.group(1) not in ("none", "stopped"):
        m.run(["brew", "services", "stop", "muxa"], "stop brew service muxa")
        NEW_SERVICES.append(("brew", Path("tend"), "tend"))
    if capture(["pgrep", "-x", "muxad"]).strip():
        m.run(["pkill", "-x", "muxad"], "stop the running muxad")


# Services moved to their Tend names, started once everything else is done.
NEW_SERVICES: list[tuple[str, Path, str]] = []


def convert_services(m: Migration):
    """After the directories moved (so MOVES is known): rewrite the stopped
    Muxa jobs under their Tend names."""
    if MACOS:
        agents = HOME / "Library/LaunchAgents"
        for label in ("dev.open330.muxad", "dev.open330.muxa-restore"):
            plist = agents / f"{label}.plist"
            if plist.exists():
                move_launchd_job(m, plist, agents / f"{full_rename(label)}.plist")
    for base in (xdg("XDG_DATA_HOME", HOME / ".local/share"), xdg("XDG_CONFIG_HOME", HOME / ".config")):
        for unit in ("muxad.service", "muxa-restore.service"):
            path = base / "systemd/user" / unit
            if path.exists():
                move_systemd_unit(m, path, path.with_name(full_rename(unit)))


def move_launchd_job(m: Migration, old: Path, new: Path):
    """Keep the job's environment (a hand-tuned PATH, say) and settings;
    rename its label, program and log paths."""
    if new.exists():
        m.note(f"{new} already exists, so {old} was left in place. Remove it once tend works.")
        return
    job = plistlib.loads(old.read_bytes())
    job["Label"] = full_rename(job.get("Label", ""))
    args = job.get("ProgramArguments") or []
    if args:
        job["ProgramArguments"] = [rename_program_path(args[0])] + [surgical_rename(a) for a in args[1:]]
    if "Program" in job:
        job["Program"] = rename_program_path(job["Program"])
    if isinstance(job.get("EnvironmentVariables"), dict):
        job["EnvironmentVariables"] = {ENV_NAME.sub(r"TEND_\1", k): surgical_rename(v) if isinstance(v, str) else v
                                       for k, v in job["EnvironmentVariables"].items()}
    for key in ("StandardOutPath", "StandardErrorPath", "WorkingDirectory"):
        if isinstance(job.get(key), str):
            job[key] = surgical_rename(job[key])
    program = (job.get("ProgramArguments") or [job.get("Program", "")])[0]
    m.step(f"move launchd job {old.name} → {new.name} (runs {program})")
    if m.apply:
        new.write_bytes(plistlib.dumps(job))
        os.chmod(new, old.stat().st_mode & 0o7777)
        old.rename(old.with_name(f"{old.name}.muxa-migrate-{STAMP}"))
    NEW_SERVICES.append(("launchd", new, program))


def systemd_text(text: str) -> tuple[str, str]:
    """Rename a unit file or drop-in: Exec lines get the executable renamed by
    file name (keeping systemd quoting), everything else the surgical
    rename (MUXA_* variables, moved paths). Returns the text and the first
    executable."""
    lines, program = [], ""
    for line in text.splitlines(keepends=True):
        exec_line = re.match(r"(\s*Exec\w*=[-@:+!]*)(?:\"([^\"]*)\"|'([^']*)'|(\S+))(.*)", line, re.S)
        if exec_line:
            raw = next(g for g in exec_line.group(2, 3, 4) if g is not None)
            quote = '"' if exec_line.group(2) is not None else "'" if exec_line.group(3) is not None else ""
            renamed = rename_program_path(raw)
            line = exec_line.group(1) + quote + renamed + quote + surgical_rename(exec_line.group(5))
            program = program or renamed
        else:
            line = surgical_rename(line)
        lines.append(line)
    return "".join(lines), program


def move_systemd_unit(m: Migration, old: Path, new: Path):
    """The unit and its `<unit>.d/` drop-ins (an Environment= override, say)
    move together under the Tend name."""
    old_dropins, new_dropins = old.with_name(old.name + ".d"), new.with_name(new.name + ".d")
    if new.exists() or new_dropins.exists():
        m.note(f"{new} already exists, so {old} was left in place. Remove it once tend works.")
        return
    text, program = systemd_text(old.read_text(encoding="utf-8"))
    m.step(f"move systemd unit {old.name} → {new.name}")
    confs = sorted(old_dropins.glob("*.conf")) if old_dropins.is_dir() else []
    if old_dropins.is_dir():
        m.step(f"move its drop-ins {old_dropins.name}/ → {new_dropins.name}/ ({len(confs)} files)")
    if m.apply:
        atomic_write(new, text)
        old.rename(old.with_name(f"{old.name}.muxa-migrate-{STAMP}"))
        if old_dropins.is_dir():
            new_dropins.mkdir()
            for conf in confs:
                conf_text, conf_program = systemd_text(conf.read_text(encoding="utf-8"))
                atomic_write(new_dropins / conf.name, conf_text)
                program = conf_program or program  # a drop-in ExecStart= overrides the unit's
            old_dropins.rename(old_dropins.with_name(f"{old_dropins.name}.muxa-migrate-{STAMP}"))
    NEW_SERVICES.append(("systemd", new, program.replace("%h", str(HOME))))


def start_services(m: Migration):
    """Start the moved services once the data is in place, if Tend is
    installed where they expect it."""
    if not NEW_SERVICES:
        return
    print("\nStart Tend services")
    reloaded = False
    for kind, path, program in NEW_SERVICES:
        if program and "/" in program and not os.path.exists(os.path.expanduser(program)):
            m.note(f"{path.name} runs {program}, which is not installed yet. Install Tend, then start it "
                   + (f"with: launchctl bootstrap gui/{UID} '{path}'" if kind == "launchd"
                      else f"with: systemctl --user enable --now {path.name}"))
            continue
        if kind == "brew":
            if "tend" in capture(["brew", "list", "--formula"]).split():
                m.run(["brew", "services", "start", "tend"], "start brew service tend")
            else:
                m.note("Muxa ran as a brew service. Install Tend and start it: "
                       "brew install open330/tap/tend && brew services start tend")
            continue
        if kind == "launchd":
            m.run(["launchctl", "bootstrap", f"gui/{UID}", str(path)], f"start launchd job {path.stem}")
        else:
            if not reloaded:
                m.run(["systemctl", "--user", "daemon-reload"], "reload systemd user units")
                reloaded = True
            m.run(["systemctl", "--user", "enable", "--now", path.name], f"enable and start {path.name}")


def move_directories(m: Migration) -> list[Path]:
    print("\nDirectories")
    if MACOS:
        support = HOME / "Library/Application Support"
        data, cfg = xdg("XDG_DATA_HOME", support), xdg("XDG_CONFIG_HOME", support)
        pairs = [(data / "muxa", data / "tend"), (HOME / "Library/Caches/muxa", HOME / "Library/Caches/tend")]
        if cfg != data:
            pairs.append((cfg / "muxa", cfg / "tend"))
    else:
        pairs = [(xdg(v, d) / "muxa", xdg(v, d) / "tend") for v, d in (
            ("XDG_DATA_HOME", HOME / ".local/share"),
            ("XDG_CONFIG_HOME", HOME / ".config"),
            ("XDG_CACHE_HOME", HOME / ".cache"))]
    for src, dst in pairs:
        m.move(src, dst)
    return [dst for _, dst in pairs]


def rewrite_own_files(m: Migration, dirs: list[Path]):
    print("\nTend's own files")
    for base in dirs:
        # When only planning, nothing has moved yet: read the source.
        look = base if base.exists() else base.with_name("muxa")
        if not look.exists():
            continue
        bundle = look / "agent-integration"
        old_skill = bundle / "skills/muxa-collaboration"
        if old_skill.exists():
            m.step(f"rename {old_skill.name} → tend-collaboration in {bundle}")
            if m.apply:
                old_skill.rename(bundle / "skills/tend-collaboration")
        # config.toml and agent definitions hold user values (cwd, commands,
        # fleet destinations): rename only Muxa's own entries in them.
        m.edit(look / "config.toml", config_toml, validate=valid_toml)
        for path in sorted((look / "agents").glob("*.toml")) if (look / "agents").exists() else []:
            m.edit(path, surgical_rename, validate=valid_toml)
        m.edit_json(bundle / "manifest.json", lambda d: json_strings(d, surgical_rename))
        # The bootstrap and skill documents are Muxa's text throughout.
        m.edit(bundle / "bootstrap.md", full_rename)
        skills = bundle / "skills"
        for path in sorted(skills.rglob("*.md")) if skills.exists() else []:
            m.edit(path, full_rename)


def codex_homes(extra: list[str]) -> list[Path]:
    homes = [Path(os.environ.get("CODEX_HOME") or HOME / ".codex")] + [Path(p).expanduser() for p in extra]
    seen, out = set(), []
    for h in homes:
        if h.exists() and h.resolve() not in seen:
            seen.add(h.resolve())
            out.append(h)
    return out


def claude_homes(extra: list[str]) -> list[Path]:
    homes = [HOME / ".claude"]
    if os.environ.get("CLAUDE_CONFIG_DIR"):
        homes.append(Path(os.environ["CLAUDE_CONFIG_DIR"]))
    homes += [Path(p).expanduser() for p in extra]
    seen, out = set(), []
    for h in homes:
        if h.exists() and h.resolve() not in seen:
            seen.add(h.resolve())
            out.append(h)
    return out


def fix_mcp_servers(m: Migration, where: Path, servers):
    if not isinstance(servers, dict) or "muxa" not in servers:
        return servers
    if "tend" in servers:
        m.note(f"{where} already has an MCP server named tend; its muxa entry was left in place. Remove it once tend works.")
        return servers
    entry = servers.pop("muxa")
    if isinstance(entry, dict):
        entry = {k: (surgical_rename(rename_program_path(v)) if k == "command" and isinstance(v, str)
                     else json_strings(v, surgical_rename)) for k, v in entry.items()}
        if isinstance(entry.get("env"), dict):
            entry["env"] = {ENV_NAME.sub(r"TEND_\1", k): v for k, v in entry["env"].items()}
    servers["tend"] = entry
    return servers


MCP_HEADER = r"\[\s*mcp_servers\s*\.\s*(?:{name}|\"{name}\"|'{name}')\s*(\]|\.)"


def migrate_codex_toml(m: Migration, path: Path):
    def transform(text: str) -> str:
        has_tend = re.search(r"(?m)^\s*" + MCP_HEADER.format(name="tend"), text) is not None
        if tomllib is not None:
            with contextlib.suppress(tomllib.TOMLDecodeError):
                has_tend = "tend" in (tomllib.loads(text).get("mcp_servers") or {})
        if not has_tend:  # dotted keys: mcp_servers.muxa.command = …
            text = re.sub(r"\bmcp_servers\s*\.\s*(?:muxa|\"muxa\"|'muxa')\s*\.", "mcp_servers.tend.", text)
        out, kept_old = [], False
        for chunk in re.split(r"(?m)^(?=\s*\[)", text):
            header = chunk.lstrip().split("\n", 1)[0]
            in_mcp = re.match(r"\[\s*mcp_servers\b", header) is not None
            if re.match(MCP_HEADER.format(name="muxa"), header):
                if has_tend:
                    kept_old = True
                    out.append(chunk)  # both exist: leave the old server for the user
                    continue
                chunk = re.sub(MCP_HEADER.format(name="muxa"), r"[mcp_servers.tend\1", chunk, count=1)
            # An MCP server's command is an executable path; a hook's is a command line.
            # An MCP server's command is an executable path; a hook's is a
            # command line. Inline (`mcp_servers = { muxa = { … } }`) and
            # dotted (`mcp_servers.muxa.command = …`) spellings count as MCP.
            lines = []
            for line in chunk.splitlines(keepends=True):
                if has_tend and re.search(r"\bmcp_servers\s*\.\s*(?:muxa|\"muxa\"|'muxa')\s*\.", line):
                    kept_old = True
                    lines.append(line.replace("muxa", "\0MUXA\0"))  # left as is (restored below)
                    continue
                rename = rename_program_path if in_mcp or re.search(r"\bmcp_servers\b", line) else rename_executable
                lines.append(re.sub(r"(\bcommand\s*=\s*)([\"'])(.*?)\2",
                                    lambda mm, f=rename: mm.group(1) + mm.group(2) + f(mm.group(3)) + mm.group(2), line))
            out.append(surgical_rename("".join(lines)).replace("\0MUXA\0", "muxa"))
        if kept_old:
            m.note(f"{path} already has [mcp_servers.tend]; [mcp_servers.muxa] was left in place. Remove it once tend works.")
        return "".join(out)
    def valid(text: str) -> bool:
        """Parses, and no migrated MCP server still runs a Muxa binary."""
        if not valid_toml(text):
            return False
        servers = tomllib.loads(text).get("mcp_servers") or {}
        stale = [name for name, entry in servers.items() if name != "muxa" and isinstance(entry, dict)
                 and os.path.basename(str(entry.get("command", ""))) in ("muxa", "muxad")]
        if stale:
            m.note(f"{path}: MCP server {', '.join(stale)} would still run a Muxa binary; left unchanged. Edit it by hand.")
            return False
        return True
    m.edit(path, transform, validate=valid)


def migrate_agents(m: Migration, extra_claude: list[str], extra_codex: list[str]):
    print("\nAgent integrations")
    homes = claude_homes(extra_claude)
    for home in homes:
        def fix_settings(d):
            for key in ("hooks", "statusLine"):
                if key in d:
                    d[key] = fix_commands(d[key])
            return d
        m.edit_json(home / "settings.json", fix_settings)
        m.edit(home / "CLAUDE.md", surgical_rename)
        migrate_skill_link(m, home / "skills")
    for claude_json in dict.fromkeys([HOME / ".claude.json", *[h / ".claude.json" for h in homes]]):
        def fix(d, where=claude_json):
            if "mcpServers" in d:
                d["mcpServers"] = fix_mcp_servers(m, where, d["mcpServers"])
            for proj in (d.get("projects") or {}).values():
                if isinstance(proj, dict) and "mcpServers" in proj:
                    proj["mcpServers"] = fix_mcp_servers(m, where, proj["mcpServers"])
            return d
        m.edit_json(claude_json, fix)

    for codex in codex_homes(extra_codex):
        migrate_codex_toml(m, codex / "config.toml")
        for name in ("AGENTS.md", "AGENTS.override.md"):
            m.edit(codex / name, surgical_rename)
    migrate_skill_link(m, HOME / ".agents/skills")

    m.edit_json(HOME / ".gemini/settings.json",
                lambda d: {**d, **({"hooks": fix_commands(d["hooks"])} if "hooks" in d else {})})

    def fix_agy(d):
        if isinstance(d, dict) and "muxa" in d:
            if "tend" in d:
                m.note(f"{HOME / '.gemini/config/hooks.json'} already has tend hooks; its muxa hooks were left in place.")
            else:
                d["tend"] = fix_commands(d.pop("muxa"))
        return d
    m.edit_json(HOME / ".gemini/config/hooks.json", fix_agy)

    plugins = (HOME / "Library/Application Support" if MACOS else xdg("XDG_CONFIG_HOME", HOME / ".config")) / "opencode/plugins"
    old_plugin, new_plugin = plugins / "muxa.ts", plugins / "tend.ts"
    if old_plugin.exists():
        if new_plugin.exists():
            m.note(f"{new_plugin} already exists, so {old_plugin} was left in place. Delete it once tend works.")
        else:
            m.step(f"move {old_plugin} → {new_plugin.name}")
            if m.apply:
                atomic_write(new_plugin, full_rename(old_plugin.read_text(encoding="utf-8")))
                old_plugin.rename(old_plugin.with_name(f"muxa.ts.muxa-migrate-{STAMP}"))


def migrate_skill_link(m: Migration, skills_dir: Path):
    old = skills_dir / "muxa-collaboration"
    if not old.is_symlink():
        if old.exists():
            m.note(f"{old} is a real directory, not Muxa's symlink; left in place.")
        return
    new = skills_dir / "tend-collaboration"
    if new.exists() or new.is_symlink():
        m.note(f"{new} already exists; remove the old {old} link once tend works.")
        return
    target = surgical_rename(os.readlink(old)).replace("muxa-collaboration", "tend-collaboration")
    m.step(f"relink {new} → {target}")
    if m.apply:
        new.symlink_to(target)
        old.unlink()


def migrate_shell_and_tmux(m: Migration):
    print("\nShell and tmux configuration")
    for rc in (HOME / ".zshrc", HOME / ".zprofile", HOME / ".zshenv", HOME / ".bashrc", HOME / ".bash_profile",
               HOME / ".profile", HOME / ".config/fish/config.fish"):
        m.edit(rc, surgical_rename)
    for conf in (HOME / ".tmux.conf", xdg("XDG_CONFIG_HOME", HOME / ".config") / "tmux/tmux.conf", HOME / ".rmux.conf"):
        m.edit(conf, surgical_rename)


def migrate_live_tmux(m: Migration):
    """Copy @muxa_* user options on live sessions/windows/panes to @tend_*,
    so running Work keeps its identity, then unset the old ones."""
    print("\nLive tmux and rmux options")
    servers = [[mux] for mux in ("tmux", "rmux") if have(mux)]
    if have("tmux"):
        servers += [["tmux", "-S", os.environ[v]] for v in ("MUXA_TMUX_SOCKET", "TEND_TMUX_SOCKET") if os.environ.get(v)]
    for tmux in servers:
        sessions = capture(tmux + ["list-sessions", "-F", "#{session_id}"]).split()
        if not sessions:
            continue
        windows = capture(tmux + ["list-windows", "-a", "-F", "#{window_id}"]).split()
        panes = capture(tmux + ["list-panes", "-a", "-F", "#{pane_id}"]).split()
        targets = [([], t) for t in sessions] + [(["-w"], t) for t in windows] + [(["-p"], t) for t in panes]
        found = []
        for scope, target in targets:
            for line in capture(tmux + ["show-options"] + scope + ["-t", target]).splitlines():
                name = line.split(" ", 1)[0]
                if name.startswith(("@muxa_", "@muxa-")):
                    found.append((scope, target, name))
        if found:
            m.step(f"{' '.join(tmux)}: rename {len(found)} @muxa_* options to @tend_*")
            if m.apply:
                for scope, target, name in found:
                    value = capture(tmux + ["show-options"] + scope + ["-qv", "-t", target, name]).rstrip("\n")
                    new_name = TMUX_OPTION.sub(r"@tend\1", name)
                    subprocess.run(tmux + ["set-option"] + scope + ["-t", target, new_name, value], capture_output=True)
                    subprocess.run(tmux + ["set-option"] + scope + ["-u", "-t", target, name], capture_output=True)
        # MUXA_SOCKET, MUXA_HOST(S)… in the server's global environment are
        # inherited by every agent it starts: move them all.
        for name, value in re.findall(r"^(MUXA_[A-Z0-9_]+)=(.*)$", capture(tmux + ["show-environment", "-g"]), re.M):
            new_name, new_value = ENV_NAME.sub(r"TEND_\1", name), surgical_rename(value)
            m.step(f"{' '.join(tmux)}: {new_name}={new_value} in the global environment")
            if m.apply:
                subprocess.run(tmux + ["set-environment", "-g", new_name, new_value], capture_output=True)
                subprocess.run(tmux + ["set-environment", "-gu", name], capture_output=True)


def migrate_macos_app(m: Migration, keychain: str | None):
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
            new_key = re.sub(r"^Muxa", "Tend", re.sub(r"^muxa\.", "tend.", key))
            if new_key not in merged:
                merged[new_key] = value
                added += 1
        if added:
            m.step(f"copy {added} preferences from dev.muxa.mac to dev.tend.mac")
            if m.apply:
                with tempfile.NamedTemporaryFile(suffix=".plist", delete=False) as fh:
                    fh.write(plistlib.dumps(merged))
                subprocess.run(["defaults", "import", "dev.tend.mac", fh.name], check=True)
                os.unlink(fh.name)
    migrate_keychain(m, keychain)
    for app in (Path("/Applications/Muxa.app"), HOME / "Applications/Muxa.app"):
        if app.exists():
            m.note(f"{app} is still installed. Quit it and move it to the Trash once Tend.app works.")


def keychain_accounts(service: str, keychain: str | None) -> set[str]:
    dump = capture(["security", "dump-keychain"] + ([keychain] if keychain else []))
    accounts, item = set(), {}
    for line in dump.splitlines() + ["keychain: end"]:
        if line.startswith("keychain:"):
            if item.get("svce") == service and item.get("acct"):
                accounts.add(item["acct"])
            item = {}
        found = re.search(r'"(acct|svce)"<blob>="([^"]*)"', line)
        if found:
            item[found.group(1)] = found.group(2)
    return accounts


def security_quote(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def migrate_keychain(m: Migration, keychain: str | None):
    """Ask provider API keys move from dev.muxa.mac.ask-provider to
    dev.tend.mac.ask-provider. A key already saved under Tend wins. The
    secret goes to `security -i` on stdin, never on a command line."""
    kc = [keychain] if keychain else []
    existing = keychain_accounts(NEW_SERVICE, keychain)
    for account in sorted(keychain_accounts(OLD_SERVICE, keychain)):
        if account in existing:
            continue  # Tend already has its own key for this provider
        m.step(f"copy Ask API key {account} to the {NEW_SERVICE} keychain service")
        if not m.apply:
            continue
        secret = subprocess.run(["security", "find-generic-password", "-s", OLD_SERVICE, "-a", account, "-w"] + kc,
                                capture_output=True, text=True)
        if secret.returncode != 0:
            m.note(f"Could not read the {account} key; enter it again in Tend › Settings › Ask.")
            continue
        command = " ".join(["add-generic-password", "-s", security_quote(NEW_SERVICE), "-a", security_quote(account),
                            "-w", security_quote(secret.stdout.rstrip("\n"))] + [security_quote(k) for k in kc]) + "\n"
        added = subprocess.run(["security", "-i"], input=command, capture_output=True, text=True)
        if added.returncode != 0 or account not in keychain_accounts(NEW_SERVICE, keychain):
            m.note(f"Could not save the {account} key under Tend; enter it again in Tend › Settings › Ask.")


def report_custom_jobs(m: Migration):
    """Your own launchd jobs, systemd units and crontab entries are not
    Muxa's to rename, but one that runs muxa (directly or through a script)
    stops working once Muxa is removed: list every one that mentions it."""
    known = {"dev.open330.muxad.plist", "dev.open330.muxa-restore.plist", "muxad.service", "muxa-restore.service"}
    candidates = []
    if MACOS:
        candidates += sorted((HOME / "Library/LaunchAgents").glob("*.plist"))
    for base in (xdg("XDG_CONFIG_HOME", HOME / ".config"), xdg("XDG_DATA_HOME", HOME / ".local/share")):
        candidates += sorted((base / "systemd/user").glob("*.service")) if (base / "systemd/user").exists() else []
    for path in candidates:
        if path.name in known or ".muxa-migrate-" in path.name:
            continue
        with contextlib.suppress(OSError):
            data = path.read_bytes()
            if path.suffix == ".plist":
                with contextlib.suppress(Exception):
                    data = json.dumps(plistlib.loads(data)).encode()
            if b"muxa" in data.lower():
                m.note(f"{path} mentions muxa. It is your own job, so it was left as is; "
                       f"if it runs Muxa, point it at tend.")
    cron = capture(["crontab", "-l"]) if have("crontab") else ""
    for line in cron.splitlines():
        if not line.lstrip().startswith("#") and "muxa" in line.lower():
            m.note(f"a crontab entry mentions muxa: {line.strip()[:100]}")


def report_leftovers(m: Migration):
    for name in ("muxa", "muxad"):
        path = shutil.which(name)
        if path:
            how = ("brew uninstall muxa" if "Cellar" in os.path.realpath(path) or "homebrew" in path
                   else "cargo uninstall muxa-cli muxad, or delete the file")
            m.note(f"{path} is still on PATH. Remove it once Tend works ({how}).")
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
    parser.add_argument("--codex-home", action="append", default=[], metavar="DIR",
                        help="another Codex config dir (CODEX_HOME) to migrate (repeatable)")
    parser.add_argument("--skip-tmux", action="store_true", help="leave live tmux sessions alone")
    parser.add_argument("--only-files", action="store_true",
                        help="only move directories and rewrite files; leave services, live tmux, "
                             "macOS preferences and the keychain alone")
    parser.add_argument("--keychain", metavar="PATH", help=argparse.SUPPRESS)  # tests use a scratch keychain
    parser.add_argument("--only-keychain", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    m = Migration(apply=args.apply, verbose=args.verbose)
    print("Migrating Muxa → Tend" + ("" if args.apply else " (plan only; pass --apply to make these changes)"))
    if args.only_keychain:
        migrate_keychain(m, args.keychain)
        print(f"{m.done} keychain changes" + ("" if args.apply else " planned"))
        return 0
    if not args.only_files:
        stop_services(m)
    dirs = move_directories(m)
    if not args.only_files:
        convert_services(m)
    rewrite_own_files(m, dirs)
    migrate_agents(m, args.claude_home, args.codex_home)
    migrate_shell_and_tmux(m)
    if not (args.skip_tmux or args.only_files):
        migrate_live_tmux(m)
    if not args.only_files:
        migrate_macos_app(m, args.keychain)
        start_services(m)
    report_custom_jobs(m)
    report_leftovers(m)

    print()
    if m.done == 0:
        print("Nothing from Muxa found on this machine.")
    elif args.apply:
        print(f"Done: {m.done} changes. Backups end in .muxa-migrate-{STAMP}.")
        print("Next: open a new shell and restart your agents so their hooks and MCP server call `tend`.")
        if not NEW_SERVICES and not args.only_files:
            print("No Muxa service was found to move; run `tend init` to install the tendd service.")
    else:
        print(f"{m.done} changes planned. Run again with --apply to make them.")
    if m.notes:
        print("\nCheck by hand:")
        for note in m.notes:
            print("  - " + note)
    return 0


if __name__ == "__main__":
    sys.exit(main())
