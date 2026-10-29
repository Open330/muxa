#!/usr/bin/env python3
"""Combine production-renderer test matrices without reading live messages.

Inputs come from adaptive_delivery_cost_matrix (before/after) and
adaptive_reply_cost_matrix. Counts model-facing text, not billed tokens,
provider framing, latency, tool arguments, or recurring context tokens.
"""
import argparse
import json
from pathlib import Path


def matrix(path):
    rows = json.loads(path.read_text())["rows"]
    indexed = {(row["language"], row["target_bytes"]): row for row in rows}
    if len(indexed) != len(rows):
        raise ValueError(f"duplicate cases: {path}")
    return indexed


def report(before, after, replies):
    if before.keys() != after.keys() or before.keys() != replies.keys():
        raise ValueError("matrix cases differ")
    rows = []
    for key, old in before.items():
        new, reply = after[key], replies[key]
        if len({row["body_bytes"] for row in (old, new, reply)}) != 1:
            raise ValueError(f"body sizes differ: {key}")
        if new["body_bytes"] != new["target_bytes"]:
            raise ValueError(f"body differs from boundary target: {key}")
        useful = new["body_bytes"] + reply["reply_body_bytes"]
        previous = old["received_bytes"] + reply["before_sender_result_bytes"] + reply["reply_receipt_bytes"]
        current = new["received_bytes"] + reply["sender_result_bytes"] + reply["reply_receipt_bytes"]
        rows.append({
            "language": key[0], "target_bytes": key[1],
            "request_body_bytes": new["body_bytes"],
            "reply_body_bytes": reply["reply_body_bytes"],
            "receiver_extra_bytes": new["extra_bytes"],
            "receiver_extra_body_ratio": new["extra_body_ratio"],
            "json_body_escaping_extra_bytes": new["json_body_escaping_extra_bytes"],
            "before_received_total_bytes": previous,
            "received_total_bytes": current,
            "extra_total_bytes": current - useful,
            "extra_useful_body_ratio": (current - useful) / useful,
            "before_calls": 2 + old["initial_inbox_reads"],
            "calls": 2 + new["initial_inbox_reads"],
        })
    return {"scope": "synthetic synchronous call + optional inbox + reply; production-rendered receiver delivery, sender result, reply receipt",
            "units": "UTF-8 received text bytes; not billing or measured latency",
            "excluded": ["tool arguments", "provider framing", "recurring context", "recovery and asynchronous paths"],
            "rows": rows}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("before", "after", "replies"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    print(json.dumps(report(matrix(args.before), matrix(args.after), matrix(args.replies)),
                     ensure_ascii=False, indent=2))
