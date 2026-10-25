"""Regression checks for attribution, pairing and bounded duplicate reporting."""
import datetime as dt
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "cost_audit", Path(__file__).with_name("collaboration-cost-audit.py"))
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)


class CostAuditTests(unittest.TestCase):
    def test_pairs_provider_formats_and_keeps_ui_separate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ledger = root / "audit.ndjson"
            ledger.write_text(json.dumps({"at": "2026-10-20T01:00:00Z", "operation": "list",
                                          "actor": {"client_kind": "watch"}, "ok": True}) + "\n")
            codex = root / "codex.jsonl"
            rows = []
            for i, minute in enumerate((1, 2, 20)):
                for payload in [
                    {"type": "custom_tool_call", "call_id": str(i), "name": "exec",
                     "input": "text(ALL_TOOLS.filter(x => x.name.includes('muxa')))"},
                    {"type": "custom_tool_call_output", "call_id": str(i), "output": "한글"},
                ]:
                    rows.append({"timestamp": f"2026-10-20T01:{minute:02}:00Z",
                                 "type": "response_item", "payload": payload})
            codex.write_text("\n".join(json.dumps(row) for row in rows) + '\n{"incomplete":')
            claude = root / "claude.jsonl"
            claude.write_text("\n".join(json.dumps(row) for row in [
                {"timestamp": "2026-10-20T01:01:00Z", "type": "assistant", "message": {"content": [
                    {"type": "tool_use", "id": "one", "name": "mcp__muxa__muxa_room_context", "input": {}}]}},
                {"timestamp": "2026-10-20T01:01:01Z", "type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "one", "content": "private result"}]}},
            ]))
            report = audit.summarize(ledger, [codex, claude, codex],
                                     dt.datetime(2026, 10, 20, tzinfo=dt.timezone.utc), 300)
            self.assertEqual(report["sessions_scanned"], 2)
            self.assertEqual(report["ipc"], [{"client": "watch", "operation": "list", "calls": 1, "failed": 0}])
            by_tool = {row["tool"]: row for row in report["tool_results"]}
            discovery = by_tool["exec:muxa-discovery (mixed output)"]
            self.assertEqual(discovery["calls"], 3)
            self.assertEqual(discovery["output_bytes"], 18)  # UTF-8, not character count.
            self.assertEqual(discovery["same_result_repeats"], 1)
            self.assertEqual(discovery["same_result_repeat_bytes"], 6)
            self.assertEqual(by_tool["mcp__muxa__muxa_room_context"]["calls"], 1)
            self.assertNotIn("private result", json.dumps(report))

    def test_ipc_metrics_keep_legacy_rows_and_do_not_count_invalid_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            ledger = Path(directory) / "audit.ndjson"
            base = {"at": "2026-10-20T01:00:00Z", "actor": {"client_kind": "mcp"},
                    "operation": "list", "ok": True}
            rows = [base, dict(base, response_bytes=100, elapsed_us=2000),
                    dict(base, response_bytes=300, elapsed_us=4000),
                    dict(base, response_bytes=-1, elapsed_us=True)]
            ledger.write_text("\n".join(json.dumps(row) for row in rows))
            report = audit.summarize(ledger, [], dt.datetime(2026, 10, 20, tzinfo=dt.timezone.utc), 300)
            row = report["ipc"][0]
            self.assertEqual(row["calls"], 4)
            self.assertEqual(row["response_bytes_samples"], 2)
            self.assertEqual(row["response_bytes"], 400)
            self.assertEqual(row["max_response_bytes"], 300)
            self.assertEqual(row["elapsed_us_samples"], 2)
            self.assertEqual(row["elapsed_us"], 6000)
            self.assertEqual(row["max_elapsed_us"], 4000)

    def test_missing_audit_and_source_mentions_do_not_invent_model_calls(self):
        self.assertIsNone(audit.muxa_call("Bash", {"command": "rg muxa src/main.rs"}))
        self.assertIsNone(audit.muxa_call("unrelated_muxa_room_context", {}))
        with tempfile.TemporaryDirectory() as directory:
            report = audit.summarize(Path(directory) / "missing", [],
                                     dt.datetime(2026, 10, 20, tzinfo=dt.timezone.utc), 300)
            self.assertFalse(report["audit_available"])
            self.assertEqual(report["tool_results"], [])


if __name__ == "__main__":
    unittest.main()
