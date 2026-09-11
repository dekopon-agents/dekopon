#!/usr/bin/env python3
"""Negative controls execute the smoke's actual shipper and correlation assertions."""
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).with_name("smoke-test.sh").read_text()


def block(name):
    return SOURCE.split("<<'" + name + "'\n", 1)[1].split("\n" + name, 1)[0]


class SmokeControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.rows = [dict(daemon=daemon, trace_id=str(index) * 32, span_id=str(index) * 16)
                     for index, daemon in enumerate(("dekopond", "dekopon-brokerd"), 1)]
        for row in self.rows:
            self.write(row["daemon"] + ".log", "{}\n" + json.dumps(row) + "\n", raw=True)
        self.write("openobserve-auth-header", "Authorization: Basic fake-ingest-token", raw=True)
        self.write("shipped.json", self.rows)
        # What the broker's own log exporter delivers, beside the rows this script ships.
        self.audit = {"trace_id": "1" * 32, "span_id": "a" * 16,
                      "audit.event": "broker.decision"}
        self.response("search.json", self.rows)
        self.response("log-search.json", self.rows + [self.audit])

    def write(self, name, value, raw=False):
        (self.folder / name).write_text(value if raw else json.dumps(value))

    def response(self, name, rows, **extra):
        self.write(name, dict(hits=rows, total=len(rows), is_partial=False, **extra))

    def correlate(self):
        with patch.object(sys, "argv", ["control", str(self.folder), "1" * 32]), contextlib.redirect_stdout(io.StringIO()):
            exec(block("PYCORRELATE"), {})

    def ship(self, body=None, error=None):
        response = io.BytesIO(json.dumps(body or {
            "code": 200, "status": [{"successful": 4, "failed": 0}]}).encode())
        response.status = 200
        with patch.object(sys, "argv", ["control", str(self.folder), "1", "test"]), patch.dict(
                os.environ, {"OPENOBSERVE_ROOT_PASSWORD": "fake-password"}), patch(
                "urllib.request.urlopen", return_value=response, side_effect=error) as send:
            exec(block("PYSHIP"), {})
            return send.call_count

    def test_independent_startup_trace_is_valid(self):
        self.correlate()
        self.assertEqual(self.ship(), 1)

    def test_missing_broker_delivery_fails(self):
        self.response("log-search.json", [self.rows[0]])
        with self.assertRaisesRegex(AssertionError, "coverage"):
            self.correlate()

    def test_gateway_only_delivery_with_same_count_fails(self):
        self.response("log-search.json", [self.rows[0], self.rows[0]])
        with self.assertRaisesRegex(AssertionError, "correlation missing for dekopon-brokerd"):
            self.correlate()

    def test_missing_exported_audit_record_fails(self):
        self.response("log-search.json", self.rows)
        with self.assertRaisesRegex(AssertionError, "broker.decision"):
            self.correlate()

    def test_shipped_audit_record_does_not_stand_for_an_exported_one(self):
        self.response("log-search.json", self.rows + [dict(self.audit, daemon="dekopon-brokerd")])
        with self.assertRaisesRegex(AssertionError, "broker.decision"):
            self.correlate()

    def test_wrong_exported_span_fails(self):
        self.response("search.json", [dict(row, span_id="f" * 16) for row in self.rows])
        with self.assertRaisesRegex(AssertionError, "correlation missing"):
            self.correlate()

    def test_truncation_and_partial_results_fail(self):
        for response in ({"hits": self.rows, "total": 3},
                         {"hits": self.rows, "total": 2, "is_partial": True},
                         {"hits": self.rows * 5000, "total": 10000}):
            with self.subTest(response_size=len(response["hits"])):
                self.write("log-search.json", response)
                with self.assertRaises(AssertionError):
                    self.correlate()

    def test_ingestion_record_rejection_and_count_mismatch_fail(self):
        for status in ({"successful": 3, "failed": 1}, {"successful": 4, "failed": 1}):
            with self.subTest(status=status), self.assertRaises(AssertionError):
                self.ship({"code": 200, "status": [status]})

    def test_shipper_connection_failure_is_not_success(self):
        with self.assertRaises(ConnectionError):
            self.ship(error=ConnectionError("fixture refused"))

    def test_absent_or_invalid_native_ids_fail(self):
        for rows in ([{}], [{"trace_id": "0" * 32, "span_id": "1" * 16}, {}]):
            self.write("dekopond.log", "\n".join(map(json.dumps, rows)), raw=True)
            with self.assertRaises(AssertionError):
                self.ship()

    def test_redaction_covers_full_remote_and_shipped_records(self):
        for name in ("search.json", "log-search.json", "shipped.json"):
            original = (self.folder / name).read_text()
            for secret in ("DEKOPON_OTEL_SMOKE_CREDENTIAL_MUST_NOT_APPEAR",
                           "fake-ingest-token", "fake-password"):
                self.write(name, [{"unrelated_record": secret}])
                with patch.object(sys, "argv", ["control", str(self.folder)]), patch.dict(
                        os.environ, {"OPENOBSERVE_ROOT_PASSWORD": "fake-password"}), self.assertRaisesRegex(
                        AssertionError, "leaked"):
                    exec(block("PYREDACT"), {})
            self.write(name, original, raw=True)

    def test_credentials_are_rejected_before_shipping(self):
        for secret in ("DEKOPON_OTEL_SMOKE_CREDENTIAL_MUST_NOT_APPEAR",
                       "fake-ingest-token", "fake-password"):
            self.write("dekopond.stderr.log", secret, raw=True)
            with self.assertRaisesRegex(AssertionError, "redaction"):
                self.ship()


if __name__ == "__main__":
    unittest.main()
