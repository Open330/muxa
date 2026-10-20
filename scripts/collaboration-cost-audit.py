#!/usr/bin/env python3
"""Summarize local Muxa traffic and recorded tool-output bytes, without bodies.

Daemon audit counts are IPC operations, NOT model calls. Optional Codex/Claude
session files measure recorded tool output, NOT billable model tokens. Nothing
is sent anywhere and no live agent, mailbox, or read marker is changed.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re


def records(path):
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            try:
                value = json.loads(line)
                if isinstance(value, dict):
                    yield value
            except (ValueError, UnicodeError):
                continue  # A live writer can leave an incomplete last line.


def timestamp(value):
    try:
        return dt.datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(dt.timezone.utc)
    except (ValueError, TypeError, AttributeError):
        return None


def payload_bytes(value):
    if isinstance(value, str):
        return value.encode("utf-8")
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def muxa_call(name, arguments):
    # Exact direct tools; arbitrary source paths or prose mentioning Muxa
    # are not enough to classify an execution as a Muxa call.
    if re.fullmatch(r"(?:mcp__muxa__|muxa[.:])?muxa_[a-z_]+", name):
        return name, arguments
    raw = arguments if isinstance(arguments, str) else json.dumps(arguments)
    if name in ("exec", "functions.exec"):
        if re.search(r"tools\.mcp__muxa__muxa_\w+\s*\(", raw):
            return "exec:muxa-tools (mixed output)", raw
        if "ALL_TOOLS" in raw and "muxa" in raw:
            return "exec:muxa-discovery (mixed output)", raw
    if name in ("exec", "functions.exec", "exec_command", "functions.exec_command", "Bash"):
        if re.search(r"\bmuxa\s+(?:msg|peers|identity|reload|doctor|fleet)\b", raw):
            return "shell:muxa (mixed output)", raw
    return None


def session_events(path):
    """Yield paired calls/results for both recorded provider formats."""
    pending = {}
    for row in records(path):
        at = timestamp(row.get("timestamp"))
        item = row.get("payload", {})
        if row.get("type") == "response_item":
            if item.get("type") in ("function_call", "custom_tool_call"):
                pending[item.get("call_id")] = (at, item.get("name", ""), item.get("arguments", item.get("input", {})))
            elif item.get("type") in ("function_call_output", "custom_tool_call_output"):
                call = pending.pop(item.get("call_id"), None)
                if call:
                    yield (at or call[0], call[1], call[2], item.get("output", ""))
        message = row.get("message", {})
        content = message.get("content", []) if isinstance(message, dict) else []
        if not isinstance(content, list):
            continue
        for item in content:
            if not isinstance(item, dict):
                continue
            if item.get("type") == "tool_use":
                pending[item.get("id")] = (at, item.get("name", ""), item.get("input", {}))
            elif item.get("type") == "tool_result":
                call = pending.pop(item.get("tool_use_id"), None)
                if call:
                    yield (at or call[0], call[1], call[2], item.get("content", ""))


def summarize(audit, sessions, since, repeat_seconds):
    ipc = collections.Counter()
    ipc_failed = collections.Counter()
    unknown_at = 0
    if audit.exists():
        for row in records(audit):
            at = timestamp(row.get("at"))
            if at is None or at < since:
                continue
            client = row.get("actor", {}).get("client_kind", "unknown")
            key = (client, row.get("operation", "unknown"))
            ipc[key] += 1
            if row.get("ok") is False:
                ipc_failed[key] += 1
    stats = {}
    seen_files = set()
    for path in sessions:
        resolved = path.resolve()
        if resolved in seen_files:
            continue
        seen_files.add(resolved)
        previous = {}
        for at, name, arguments, output in session_events(path):
            call = muxa_call(name, arguments)
            if call is None:
                continue
            if at is None:
                unknown_at += 1
                continue
            if at < since:
                continue
            label, arguments = call
            stat = stats.setdefault(label, {"tool": label, "calls": 0, "output_bytes": 0,
                                           "max_output_bytes": 0, "same_result_repeats": 0,
                                           "same_result_repeat_bytes": 0})
            data = payload_bytes(output)
            stat["calls"] += 1
            stat["output_bytes"] += len(data)
            stat["max_output_bytes"] = max(stat["max_output_bytes"], len(data))
            # Compare normalized arguments, only within this session and a
            # short window. Repeated output is a review candidate, not a
            # claim that a necessary status check was wasted.
            if isinstance(arguments, str):
                try:
                    arguments = json.loads(arguments)
                except ValueError:
                    pass
            signature = (label, hashlib.sha256(payload_bytes(arguments)).digest())
            digest = hashlib.sha256(data).digest()
            last = previous.get(signature)
            if last and last[1] == digest and 0 <= (at - last[0]).total_seconds() <= repeat_seconds:
                stat["same_result_repeats"] += 1
                stat["same_result_repeat_bytes"] += len(data)
            previous[signature] = (at, digest)
    return {
        "since": since.isoformat(),
        "units": "UTF-8 bytes of recorded tool results; not billed tokens",
        "caveats": ["IPC calls include watch/UI traffic, which is not model input.",
                    "Mixed execution outputs can include unrelated commands.",
                    "Identical short-window results are candidates for review, not proven waste.",
                    "Only supplied session files are measured; provider caching/tokenization is unknown."],
        "audit_available": audit.exists(),
        "ipc": [{"client": c, "operation": op, "calls": count, "failed": ipc_failed[(c, op)]}
                for (c, op), count in sorted(ipc.items())],
        "sessions_scanned": len(seen_files),
        "unmeasured_calls_without_timestamp": unknown_at,
        "tool_results": sorted(stats.values(), key=lambda s: s["output_bytes"], reverse=True),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    data_home = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    parser.add_argument("--audit", type=Path, default=data_home / "muxa/collaboration-audit.ndjson")
    parser.add_argument("--session", type=Path, action="append", default=[],
                        help="Codex or Claude JSONL; repeat for several sessions")
    parser.add_argument("--hours", type=float, default=24)
    parser.add_argument("--repeat-seconds", type=float, default=300)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if not 0 < args.hours < float("inf") or not 0 < args.repeat_seconds < float("inf"):
        parser.error("hours and repeat-seconds must be finite and positive")
    try:
        since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=args.hours)
        report = summarize(args.audit, args.session, since, args.repeat_seconds)
    except (OSError, OverflowError) as error:
        parser.exit(1, f"audit failed: {error}\n")
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return
    print(f"Since {report['since']}; recorded output bytes, NOT billed tokens")
    print("IPC calls (watch/UI calls do not imply model token costs):")
    if not report["audit_available"]:
        print("  Audit file unavailable; IPC usage is unknown, not zero.")
    for row in report["ipc"]:
        print(f"  {row['client']:8} {row['operation']:12} {row['calls']:6} calls {row['failed']:4} failed")
    print(f"Tool results from {report['sessions_scanned']} supplied sessions:")
    for row in report["tool_results"]:
        print(f"  {row['tool']}: {row['calls']} calls, {row['output_bytes']} bytes, "
              f"max {row['max_output_bytes']}; identical repeats {row['same_result_repeats']} "
              f"({row['same_result_repeat_bytes']} bytes)")
    if not args.session:
        print("  Supply --session PATH for actual recorded model-facing tool outputs.")
    for caveat in report["caveats"]:
        print(f"Note: {caveat}")


if __name__ == "__main__":
    main()
