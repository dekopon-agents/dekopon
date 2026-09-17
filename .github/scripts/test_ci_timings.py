#!/usr/bin/env python3
"""Focused fixtures: no network, credentials, or retained real logs."""

import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from ci_timings_analysis import derive, execution_evidence, workflow_references
from ci_timings import (
    stamp,
    Gh, LogMetrics, SafeError, analyze, attempt_jobs, collect, duration,
    freeze, main, map_job, run_identity, source_jobs, summary,
)

START = "2026-01-01T00:00:00Z"
END = "2026-01-01T00:01:00Z"
SHA = "a" * 40


def step(number=1, name="Check out source", conclusion="success"):
    return dict(number=number, name=name, conclusion=conclusion,
                status="completed", started_at=START, completed_at=END)


class FakeApi(Gh):
    def __init__(self, responses):
        super().__init__()
        self.responses = responses
        self.endpoints = []

    def request(self, endpoint, parser=None):
        self.endpoints.append(endpoint)
        value = self.responses[endpoint]
        if isinstance(value, Exception):
            raise value
        if parser:
            for line in value:
                parser.feed(line)
            return None
        return value


class CiTimingsTests(unittest.TestCase):
    def test_run_url_id_and_attempt_url(self):
        self.assertEqual(run_identity("12", "a/b"), ("a/b", 12))
        self.assertEqual(run_identity("https://github.com/a/b/actions/runs/12/attempts/2", "x/y"), ("a/b", 12))
        with self.assertRaisesRegex(SafeError, "invalid_run"):
            run_identity("https://example.com/a/b/actions/runs/12", "a/b")

    def test_pagination_preserves_all_pages(self):
        api = FakeApi({"runs?per_page=100&page=1": {"runs": list(range(100))},
                       "runs?per_page=100&page=2": {"runs": [100]}})
        self.assertEqual(list(api.pages("runs", "runs")), list(range(101)))

    def test_freeze_includes_cancelled_failed_and_does_not_refresh(self):
        endpoint = "repos/a/b/actions/workflows/ci.yml/runs?status=completed&per_page=100&page=1"
        runs = [dict(id=i, run_attempt=1, created_at=START, conclusion=c)
                for i, c in enumerate(("cancelled", "failure", "success"), 1)]
        api = FakeApi({endpoint: {"workflow_runs": runs}})
        with tempfile.TemporaryDirectory() as tmp:
            frozen = freeze(api, "a/b", "ci.yml", 3, Path(tmp))
            self.assertEqual([r["run_id"] for r in frozen["frozen_runs"]], [1, 2, 3])
            self.assertEqual(freeze(api, "a/b", "ci.yml", 3, Path(tmp)), frozen)
            self.assertEqual(len(api.endpoints), 1)

    def test_attempts_do_not_accept_previous_jobs_or_duplicate_ids(self):
        endpoint = "repos/a/b/actions/runs/1/attempts/2/jobs?per_page=100&page=1"
        api = FakeApi({endpoint: {"jobs": [dict(id=1, run_attempt=1)]}})
        with self.assertRaisesRegex(SafeError, "job_attempt"):
            attempt_jobs(api, "a/b", 1, 2)
        api.responses[endpoint] = {"jobs": [dict(id=1, run_attempt=2)] * 2}
        with self.assertRaisesRegex(SafeError, "identity"):
            attempt_jobs(api, "a/b", 1, 2)

    def test_null_skipped_cancelled_and_real_zero_durations(self):
        self.assertEqual(duration(START, END, "cancelled"), 60)
        self.assertEqual(duration(START, START), 0)
        for start, end, conclusion in ((START, END, "skipped"), (START, None, "cancelled"),
                                       (None, END, None), (END, START, None),
                                       ("0001-01-01T00:00:00Z", END, None)):
            self.assertIsNone(duration(start, end, conclusion))

    def test_numeric_log_allowlist_and_checkout_provenance(self):
        parser = LogMetrics([step()])
        parser.feed("2026-01-01T00:00:01Z [command]/usr/bin/git log -1 --format=%H")
        parser.feed("2026-01-01T00:00:02Z " + SHA)
        self.assertEqual(parser.checkouts, {SHA})
        parser.feed("2026-01-01T00:00:03Z     Finished `test` profile [unoptimized] target(s) in 1m 2.50s")
        parser.feed("2026-01-01T00:00:04Z test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s")
        parser.feed("2026-01-01T00:00:05Z Cache hits                            12")
        parser.feed("2026-01-01T00:00:06Z Cache restored from key: SECRET_SENTINEL")
        parser.feed("2026-01-01T00:00:07Z arbitrary SECRET_SENTINEL")
        rows = parser.rows(99)
        self.assertEqual(len(rows), 4)
        self.assertNotIn("SECRET", json.dumps(rows))
        self.assertEqual(next(r for r in rows if r["metric"] == "cargo_finished_seconds")["total"], 62.5)

    def test_checkout_sha_requires_immediate_line_in_checkout_step(self):
        parser = LogMetrics([step(name="Run tests")])
        parser.feed("2026-01-01T00:00:01Z [command]/usr/bin/git log -1 --format=%H")
        parser.feed("2026-01-01T00:00:02Z " + SHA)
        self.assertFalse(parser.checkouts)
        parser = LogMetrics([step()])
        parser.feed("2026-01-01T00:00:01Z [command]/usr/bin/git log -1 --format=%H")
        parser.feed("2026-01-01T00:00:02Z not-a-sha")
        parser.feed("2026-01-01T00:00:03Z " + SHA)
        self.assertFalse(parser.checkouts)

    def test_checkout_completion_second_has_explicit_precision_tolerance(self):
        parser = LogMetrics([step()])
        parser.feed("2026-01-01T00:01:00.100Z [command]/usr/bin/git log -1 --format=%H")
        parser.feed("2026-01-01T00:01:00.200Z " + SHA)
        self.assertEqual(parser.checkouts, {SHA})
        parser = LogMetrics([step()])
        parser.feed("2026-01-01T00:01:01Z [command]/usr/bin/git log -1 --format=%H")
        parser.feed("2026-01-01T00:01:01.100Z " + SHA)
        self.assertFalse(parser.checkouts)

    def test_log_timestamp_boundary_and_ambiguity_are_unmapped(self):
        parser = LogMetrics([step(), step(2)])
        parser.feed("2026-01-01T00:00:01Z Cache hits 2")
        self.assertIsNone(parser.rows(1)[0]["step_number"])
        parser = LogMetrics([step()])
        parser.feed("2026-01-01T00:01:00Z Cache hits 2")
        self.assertIsNone(parser.rows(1)[0]["step_number"])

    def test_historical_source_job_identity_and_unknown_syntax(self):
        source = "jobs:\n  build:\n    name: Build\n    steps:\n      - name: Test\n        run: cargo test\n  other:\n    name: Other\n    steps:\n      - name: Test\n        run: true\n"
        definitions = source_jobs(source)
        self.assertEqual(map_job("Build", definitions)["steps"][0]["line"], 5)
        self.assertEqual(map_job("Other", definitions)["steps"][0]["line"], 10)
        self.assertIsNone(map_job("Generated (matrix)", definitions))

    def test_missing_logs_do_not_block_api_timings_and_attempts_stay_distinct(self):
        run = dict(id=1, run_number=2, run_attempt=2, status="completed", conclusion="success",
                   event="pull_request", head_sha=SHA, created_at=START, updated_at=END,
                   run_started_at=START, path=".github/workflows/ci.yml")
        responses = {"repos/a/b/actions/runs/1": run,
                     f"repos/a/b/contents/.github/workflows/ci.yml?ref={SHA}": SafeError("http_404")}
        for attempt in (1, 2):
            start = START if attempt == 1 else "2026-01-01T00:02:00Z"
            end = END if attempt == 1 else "2026-01-01T00:03:00Z"
            responses[f"repos/a/b/actions/runs/1/attempts/{attempt}"] = dict(run, run_started_at=start)
            responses[f"repos/a/b/actions/runs/1/attempts/{attempt}/jobs?per_page=100&page=1"] = {"jobs": [
                dict(id=attempt, run_attempt=attempt, name="Build", status="completed", conclusion="success",
                     started_at=start, completed_at=end, steps=[dict(step(), started_at=start, completed_at=end)])]}
            responses[f"repos/a/b/actions/jobs/{attempt}/logs"] = SafeError("http_404")
        manifest = dict(repo="a/b", frozen_runs=[dict(run_id=1, latest_attempt=2)])
        with tempfile.TemporaryDirectory() as tmp, contextlib.redirect_stdout(io.StringIO()):
            output = Path(tmp)
            collect(FakeApi(responses), manifest, output)
            report = analyze(output)
            self.assertEqual(report["counts"]["attempts"], 2)
            self.assertEqual(report["counts"]["jobs"], 2)
            self.assertEqual(report["log_coverage"], {"http_404": 2})
            self.assertEqual(report["prior_attempt_job_seconds"], 60)
            self.assertEqual(report["jobs_timings"][0]["total"], 120)

    def test_schema_errors_record_failed_collection_without_response_content(self):
        for error in (KeyError("SECRET_FIELD"), ValueError("SECRET_VALUE"), TypeError("SECRET_TYPE")):
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as tmp:
                manifest = dict(repo="a/b", frozen_runs=[dict(run_id=1, latest_attempt=1)])
                api = FakeApi({"repos/a/b/actions/runs/1": error})
                with self.assertRaisesRegex(SafeError, "^invalid_response_schema$"):
                    collect(api, manifest, Path(tmp))
                saved = json.loads((Path(tmp) / "manifest.json").read_text())
                self.assertEqual(saved["state"], "failed")
                self.assertEqual(saved["errors"], [dict(run_id=1, code="invalid_response_schema")])
                self.assertIn("collection_finished_at", saved)
                self.assertNotIn("SECRET", json.dumps(saved))

    def test_error_body_and_stderr_never_escape(self):
        # Real pipe transport, synthetic command instead of gh; response body is secret-like.
        import subprocess
        real_popen = subprocess.Popen
        def fake_popen(*args, **kwargs):
            return real_popen(["python3", "-c", "import sys; print('HTTP/2.0 404 Not Found\\n\\nSECRET_BODY'); print('SECRET_STDERR', file=sys.stderr)"], **kwargs)
        parser = LogMetrics([step()])
        with patch("ci_timings.subprocess.Popen", side_effect=fake_popen):
            with self.assertRaisesRegex(SafeError, "^http_404$"):
                Gh().request("unused", parser)
        self.assertEqual(parser.rows(1), [])

    def test_ansi_log_transport_is_allowed_only_into_private_parser_pipe(self):
        import subprocess
        real_popen = subprocess.Popen
        def fake_popen(command, **kwargs):
            self.assertIn("--allow-escape-sequences", command)
            self.assertEqual(kwargs["stdout"], subprocess.PIPE)
            script = "print('HTTP/2.0 200 OK\\n\\n2026-01-01T00:00:01Z \\x1b[32mCache hits 2\\x1b[0m')"
            return real_popen(["python3", "-c", script], **kwargs)
        parser = LogMetrics([step()])
        with patch("ci_timings.subprocess.Popen", side_effect=fake_popen):
            Gh().request("unused", parser)
        self.assertEqual(parser.rows(1)[0]["metric"], "sccache_cache_hits")
        self.assertEqual(parser.rows(1)[0]["total"], 2)

    def test_retry_is_bounded_and_permanent_errors_are_not_retried(self):
        with patch.object(Gh, "_request", side_effect=SafeError("http_503")) as request, patch("ci_timings.time.sleep"):
            with self.assertRaisesRegex(SafeError, "http_503"):
                Gh().request("unused")
            self.assertEqual(request.call_count, 3)
        with patch.object(Gh, "_request", side_effect=SafeError("http_403")) as request:
            with self.assertRaisesRegex(SafeError, "http_403"):
                Gh().request("unused")
            self.assertEqual(request.call_count, 1)

    def test_transport_timeout_kills_and_reaps_child(self):
        import subprocess
        real_popen = subprocess.Popen
        children = []
        def fake_popen(command, **kwargs):
            child = real_popen(["python3", "-c", "import time; time.sleep(5)"], **kwargs)
            children.append(child)
            return child
        with patch("ci_timings.subprocess.Popen", side_effect=fake_popen), patch("ci_timings.TIMEOUT", 0.01):
            with self.assertRaisesRegex(SafeError, "timeout"):
                Gh()._request("unused", None)
        self.assertIsNotNone(children[0].poll())

    def test_log_response_byte_limit_does_not_surface_body(self):
        import subprocess
        real_popen = subprocess.Popen
        def fake_popen(command, **kwargs):
            return real_popen(["python3", "-c", "print('HTTP/2.0 200 OK\\n\\n' + 'SECRET' * 1000)"], **kwargs)
        with patch("ci_timings.subprocess.Popen", side_effect=fake_popen), patch("ci_timings.MAX_LOG", 100):
            with self.assertRaisesRegex(SafeError, "^response_byte_limit$"):
                Gh()._request("unused", LogMetrics([step()]))

    def test_cli_schema_error_is_redacted(self):
        out = io.StringIO()
        with patch("ci_timings.analyze", side_effect=ValueError("SECRET")), contextlib.redirect_stderr(out):
            self.assertEqual(main(["analyze", "unused"]), 1)
        self.assertNotIn("SECRET", out.getvalue())
        self.assertIn("local_io_or_schema_error", out.getvalue())

    def test_execution_carryovers_new_ids_are_not_new_work(self):
        later = "2026-01-01T00:02:00Z"
        jobs = [dict(run_id="1", attempt="1", job_id="10", source_job="test", name="Test (x64)", labels="linux;x64",
                     started_at=START, completed_at=END, conclusion="success")]
        jobs += [dict(jobs[0], job_id="20", attempt="2"),
                 dict(jobs[0], job_id="21", attempt="2", started_at=later, completed_at="2026-01-01T00:03:00Z"),
                 dict(jobs[0], job_id="22", attempt="2", labels="linux;arm", name="Test (arm)"),
                 dict(jobs[0], job_id="23", attempt="2", started_at="", completed_at=""),
                 dict(jobs[0], job_id="24", attempt="2", name="Test (other-feature)"),
                 dict(jobs[0], job_id="25", attempt="1", started_at="", completed_at="")]
        tables = dict(jobs=jobs, attempts=[dict(run_id="1", attempt="1", started_at=START),
                                          dict(run_id="1", attempt="2", started_at=later)],
                      steps=[dict(job_id=j["job_id"]) for j in jobs], metrics=[dict(job_id=j["job_id"]) for j in jobs])
        filtered, records = execution_evidence(tables, stamp)
        records = {r["job_id"]: r for r in records}
        self.assertEqual(records["20"]["classification"], "carryover")
        self.assertEqual(records["20"]["canonical_job_id"], "10")
        self.assertEqual(records["21"]["classification"], "canonical_observed")
        self.assertEqual(records["22"]["classification"], "uncertain_pre_attempt")
        self.assertEqual(records["23"]["classification"], "unmeasured")
        self.assertEqual(records["24"]["classification"], "uncertain_pre_attempt")
        self.assertEqual(records["25"]["classification"], "unmeasured")
        for table in ("jobs", "steps", "metrics"):
            self.assertEqual([r["job_id"] for r in filtered[table]], ["10", "21", "23", "25"])
        separate_run = dict(jobs[0], run_id="2", job_id="30")
        tables["jobs"].append(separate_run)
        tables["attempts"].append(dict(run_id="2", attempt="1", started_at=START))
        _, records = execution_evidence(tables, stamp)
        self.assertEqual(next(r for r in records if r["job_id"] == "30")["classification"], "canonical_observed")

    def test_workflow_references_keep_historical_dependencies(self):
        source = "jobs:\n  changes:\n    steps:\n      - name: Classify\n        run: true\n  test:\n    needs: changes\n    steps:\n      - name: Test\n        run: cargo test\n  quality:\n    needs: [changes, test]\n    steps:\n      - name: Gate\n        run: true\n"
        refs = workflow_references({"historical": source}, source_jobs)["historical"]
        self.assertEqual(refs["quality"]["needs"], ["changes", "test"])
        self.assertEqual(refs["test"]["steps"][0], dict(name="Test", line=9, end_line=10))

    def test_cohorts_control_conditional_lane_and_cache_maxima(self):
        tables = dict(runs=[], attempts=[], jobs=[], steps=[], metrics=[], frozen_runs=[])
        for n in range(1, 6):
            identity = str(n)
            tables["runs"].append(dict(run_id=identity, latest_attempt="1", extraction="complete"))
            tables["frozen_runs"].append(dict(run_id=identity))
            tables["attempts"].append(dict(run_id=identity, attempt="1", source_id="historical", conclusion="success", elapsed_seconds="60", started_at=START))
            tables["jobs"].append(dict(job_id=identity, run_id=identity, attempt="1", source_job="package", name="Package", labels="linux", conclusion="success", started_at=START, completed_at=END, duration_seconds=str(n)))
            tables["steps"].append(dict(job_id=identity, name="Package", number="1", source_line="10", duration_seconds="" if n == 5 else "0"))
        tables["metrics"] = [dict(job_id="1", metric="sccache_cache_hits", maximum="10", total="20", count="2"),
                             dict(job_id="1", metric="sccache_cache_hits", maximum="10", total="10", count="1")]
        result = derive(tables, {}, summary, duration)
        cohorts = sorted(result["controlled_cohorts"], key=lambda c: c["n"])
        self.assertEqual([c["n"] for c in cohorts], [1, 4])
        self.assertEqual(cohorts[1]["early"]["median"], 1.5)
        self.assertEqual(cohorts[1]["recent"]["median"], 3.5)
        self.assertEqual(result["cache"]["sccache_job_maxima"][0]["total"], 10)

    def test_inherited_dependency_gap_starts_at_rerun_not_old_finish(self):
        later = "2026-01-01T00:02:00Z"
        job = dict(run_id="1", attempt="2", job_id="20", source_job="test", name="Test", labels="linux",
                   started_at="2026-01-01T00:02:05Z", completed_at="2026-01-01T00:03:05Z", duration_seconds="60", conclusion="success")
        inherited = dict(job, job_id="10", source_job="changes", started_at=START, completed_at=END)
        tables = dict(runs=[dict(run_id="1", latest_attempt="2", created_at=START, conclusion="success", extraction="complete")],
                      attempts=[dict(run_id="1", attempt="2", source_id="historical", conclusion="success", elapsed_seconds="65", started_at=later, completed_at=job["completed_at"])],
                      jobs=[job], observed_jobs=[inherited, job], steps=[], metrics=[], frozen_runs=[dict(run_id="1")])
        refs = {"historical": {"test": {"needs": ["changes"]}}}
        result = derive(tables, refs, summary, duration)
        self.assertEqual(result["successful_latest_dependency_gaps"]["test"]["median"], 5)
        self.assertIsNone(result["observations"][0]["branch_tail_seconds"])

    def test_empty_analysis_branch_is_serializable(self):
        tables = dict(runs=[dict(run_id="1", latest_attempt="1", extraction="complete")],
                      attempts=[dict(run_id="1", attempt="1", source_id="", conclusion="success", elapsed_seconds="0")],
                      jobs=[], steps=[], metrics=[], frozen_runs=[dict(run_id="1")])
        result = derive(tables, {}, summary, duration)
        json.dumps(result, sort_keys=True)
        self.assertEqual(result["elapsed"]["all_attempts"]["branch_terminals"], {"unobserved": 1})

    def test_summary_arithmetic_and_empty_values(self):
        self.assertEqual(summary([1, 2, 3, 4]), dict(n=4, total=10, median=2.5, p90=4))
        self.assertEqual(summary([]), dict(n=0, total=0, median=None, p90=None))


if __name__ == "__main__":
    unittest.main()
