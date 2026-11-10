#!/usr/bin/env python3
"""Summarize local Tend traffic and recorded tool-output bytes, without bodies.

Daemon audit counts are IPC operations, NOT model calls. Optional Codex/Claude
session files measure recorded tool output, NOT billable model tokens. Nothing
is sent anywhere and no live agent, mailbox, or read marker is changed.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
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


def tend_call(name, arguments):
    # Exact direct tools; arbitrary source paths or prose mentioning Tend
    # are not enough to classify an execution as a Tend call.
    if re.fullmatch(r"(?:mcp__tend__|tend[.:])?tend_[a-z_]+", name):
        return name, arguments
    raw = arguments if isinstance(arguments, str) else json.dumps(arguments)
    if name in ("exec", "functions.exec"):
        if re.search(r"tools\.mcp__tend__tend_\w+\s*\(", raw):
            return "exec:tend-tools (mixed output)", raw
        if "ALL_TOOLS" in raw and "tend" in raw:
            return "exec:tend-discovery (mixed output)", raw
    if name in ("exec", "functions.exec", "exec_command", "functions.exec_command", "Bash"):
        if re.search(r"\btend\s+(?:msg|peers|identity|reload|doctor|fleet)\b", raw):
            return "shell:tend (mixed output)", raw
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
    ipc_metrics = collections.defaultdict(collections.Counter)
    unknown_at = 0
    shapes = {}
    if audit.exists():
        for row in records(audit):
            at = timestamp(row.get("at"))
            if at is None or at < since:
                continue
            client = row.get("actor", {}).get("client_kind", "unknown")
            key = (client, row.get("operation", "unknown"))
            ipc[key] += 1
            # Split response shapes: a large preview must not hide small badge reads.
            view = row.get("read_view")
            view = next(iter(view), "unknown") if isinstance(view, dict) else view
            view = view if isinstance(view, str) else "unknown"
            limit = row.get("query_limit")
            limit = limit if isinstance(limit, int) and not isinstance(limit, bool) and limit >= 0 else None
            status, scope = row.get("status"), row.get("scope")
            status = status if isinstance(status, str) else None
            scope = scope if isinstance(scope, str) else None
            shape_key = (client, key[1], view, limit, status, scope)
            shape = shapes.setdefault(shape_key, {"client": client, "operation": key[1],
                "view": view, "query_limit": limit, "status": status, "scope": scope, "calls": 0, "response_bytes": 0,
                "response_bytes_samples": 0, "elapsed_samples": []})
            shape["calls"] += 1
            wire, elapsed = row.get("response_bytes"), row.get("elapsed_us")
            if isinstance(wire, int) and not isinstance(wire, bool) and wire >= 0:
                shape["response_bytes"] += wire
                shape["response_bytes_samples"] += 1
            if isinstance(elapsed, int) and not isinstance(elapsed, bool) and elapsed >= 0:
                shape["elapsed_samples"].append(elapsed)
            for field in ("request_bytes", "response_bytes", "elapsed_us", "message_bytes", "response_body_bytes", "response_body_json_bytes", "response_unique_body_bytes", "response_duplicate_body_bytes", "response_extra_unique_bytes",
                          "response_extra_bytes", "response_structure_bytes", "response_escape_bytes"):
                value = row.get(field)
                if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
                    metrics = ipc_metrics[key]
                    metrics[field] += value
                    metrics[field + "_samples"] += 1
                    metrics["max_" + field] = max(metrics["max_" + field], value)
            # Paired samples only: old ledger rows cannot supply request framing costs.
            wire, body = row.get("request_bytes"), row.get("message_bytes")
            if isinstance(wire, int) and not isinstance(wire, bool) and isinstance(body, int) and not isinstance(body, bool) and 0 <= body <= wire:
                metrics = ipc_metrics[key]
                metrics["request_body_bytes"] += body
                metrics["request_extra_bytes"] += wire - body
                metrics["request_cost_samples"] += 1
            if isinstance(row.get("conditional_unchanged"), bool):
                ipc_metrics[key]["conditional_samples"] += 1
                ipc_metrics[key]["conditional_hits"] += int(row["conditional_unchanged"])
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
            call = tend_call(name, arguments)
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
    ipc_rows = []
    for (client, operation), count in sorted(ipc.items()):
        metrics = dict(ipc_metrics[(client, operation)])
        if metrics.get("request_cost_samples", 0):
            body = metrics["request_body_bytes"]
            metrics["request_extra_per_body_byte"] = metrics["request_extra_bytes"] / body if body else None
        if metrics.get("response_body_bytes_samples", 0):
            body = metrics.get("response_body_bytes", 0)
            metrics["extra_per_body_byte"] = metrics.get("response_extra_bytes", 0) / body if body else None
        if metrics.get("response_unique_body_bytes_samples", 0):
            unique = metrics.get("response_unique_body_bytes", 0)
            metrics["extra_per_unique_body_byte"] = metrics.get("response_extra_unique_bytes", 0) / unique if unique else None
        ipc_rows.append({"client": client, "operation": operation, "calls": count,
                         "failed": ipc_failed[(client, operation)], **metrics})
    shape_rows = []
    for shape in shapes.values():
        elapsed = sorted(shape.pop("elapsed_samples"))
        shape["elapsed_us_samples"] = len(elapsed)
        shape["p50_elapsed_us"] = elapsed[math.ceil(len(elapsed) * .5) - 1] if elapsed else None
        shape["p95_elapsed_us"] = elapsed[math.ceil(len(elapsed) * .95) - 1] if elapsed else None
        shape["elapsed_includes_wait"] = shape["operation"] == "wait"
        shape_rows.append(shape)
    return {
        "since": since.isoformat(),
        "units": "UTF-8 bytes of recorded tool results; not billed tokens",
        "caveats": ["IPC calls include watch/UI traffic, which is not model input.",
                    "IPC bytes are daemon wire responses, not final MCP/CLI output or billed tokens; timings exclude audit IO and socket writes.",
                    "Mixed execution outputs can include unrelated commands.",
                    "Body fields count request/reply/update/late/decision text after projection; raw UTF-8 and escaped JSON content exclude quotes.",
                    "extra_per_body_byte compares total IPC bytes beyond raw body text to raw body bytes; metadata is not inherently waste. Zero-body responses have no ratio.",
                    "Legacy or downgraded responses without body counters are unmeasured, not zero-cost.",
                    "Identical short-window results are candidates for review, not proven waste.",
                    "Only supplied session files are measured; provider caching/tokenization is unknown.",
                    "IPC shapes use recorded view, limit, status and scope; human_only and cursor/page position are not recorded."],
        "audit_available": audit.exists(),
        "ipc": ipc_rows,
        "ipc_shapes": sorted(shape_rows, key=lambda row: row["response_bytes"], reverse=True),
        "sessions_scanned": len(seen_files),
        "unmeasured_calls_without_timestamp": unknown_at,
        "tool_results": sorted(stats.values(), key=lambda s: s["output_bytes"], reverse=True),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    data_home = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    parser.add_argument("--audit", type=Path, default=data_home / "tend/collaboration-audit.ndjson")
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
        if row.get("response_bytes_samples"):
            print(f"    IPC responses: {row['response_bytes']} bytes / {row['response_bytes_samples']} measured calls")
        if row.get("elapsed_us_samples"):
            print(f"    request elapsed: mean {row['elapsed_us'] / row['elapsed_us_samples'] / 1000:.2f} ms, "
                  f"max {row['max_elapsed_us'] / 1000:.2f} ms / {row['elapsed_us_samples']} measured calls")
    print("Largest IPC response shapes (request elapsed includes long-poll waiting):")
    for row in report["ipc_shapes"][:10]:
        p95 = row["p95_elapsed_us"]
        latency = f"{p95 / 1000:.2f} ms" if p95 is not None else "unmeasured"
        print(f"  {row['client']}/{row['operation']} view={row['view']} limit={row['query_limit']} status={row['status']} scope={row['scope']}: "
              f"{row['calls']} calls, {row['response_bytes']} bytes "
              f"({row['response_bytes_samples']} measured), p95 {latency}")
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
