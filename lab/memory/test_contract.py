import csv
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

LAB = Path(__file__).resolve().parent


class ExitContract(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.lab = self.root / "lab"
        shutil.copytree(LAB, self.lab, ignore=shutil.ignore_patterns(".work", "runs", "__pycache__"))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.recorded = self.root / "recorded"
        self.recorded.mkdir()
        self.meta = {"ref": "main", "sha": "a" * 40, "scenario": "summarizer-hang",
                     "summarizer_timeout_ms": 20000, "profile": "fake", "build": "debug",
                     "status": "completed", "uids": {"broker": 1, "gateway": 2, "separation": "separate UIDs"},
                     "started_utc": "2026-10-26", "overlay": None}
        self.record(16000)
        self.stub("git", '''case "$*" in
          *--show-toplevel*) echo "$STUB_ROOT" ;;
          *rev-parse*) printf '%040d\\n' 0 ;;
          *worktree*) mkdir -p "${@: -2:1}/examples/providers"; touch "${@: -2:1}/examples/providers/cli-probe-provider.wasm" ;;
        esac''')
        self.stub("cargo", '''[ "${STUB_CARGO_FAIL:-0}" != 1 ] || exit 17
        while [ "$1" != --target-dir ]; do shift; done
        mkdir -p "$2/debug"
        touch "$2/debug/dekopon-brokerd" "$2/debug/dekopon-gatewayd"''')
        self.stub("docker", '''[ "${STUB_DOCKER_FAIL:-0}" != 1 ] || exit 18
        [ "$1" != container ] || exit 1''')
        self.stub("curl", "exit 0")
        self.stub("id", "echo 1000")
        self.stub("python3", '''case "$1" in
          */lab.py)
            shift
            while [ "$1" != --run ]; do shift; done
            run="$2"
            [ "${STUB_SLEEP:-0}" != 1 ] || sleep 10
            [ "${STUB_LAB_FAIL:-0}" != 1 ] || exit 1
            cp "$STUB_RECORDED/"* "$run/"
            ;;
          */report.py)
            [ "${STUB_REPORT_FAIL:-0}" != 1 ] || exit 19
            exec "$STUB_PYTHON" "$@" ;;
          *) exec "$STUB_PYTHON" "$@" ;;
        esac''')
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        STUB_ROOT=str(self.root), STUB_RECORDED=str(self.recorded),
                        STUB_PYTHON=shutil.which("python3"), LAB_WORK=str(self.root / "work"),
                        LAB_STATE_ROOT=str(self.root / "state"))
        for name in ("LAB_EXTRA_CA", "LAB_SUPERVISED", "LAB_DEADLINE_SECS", "LAB_FAULT_DROP_REPORT"):
            self.env.pop(name, None)

    def stub(self, name, script):
        file = self.bin / name
        file.write_text("#!/usr/bin/env bash\nset -e\n" + script + "\n")
        file.chmod(0o755)

    def record(self, wall):
        (self.recorded / "meta.json").write_text(json.dumps(self.meta))
        with (self.recorded / "latency.csv").open("w", newline="") as stream:
            writer = csv.writer(stream)
            writer.writerow(["kind", "conversation", "phase", "wall_ms", "summary_in_flight"])
            writer.writerow(["reply", "summary", "run", wall, "yes"])
        (self.recorded / "traces.jsonl").write_text('{"resourceSpans": []}\n')

    def run_lab(self, code, reason, *args, **env):
        result = subprocess.run(["bash", str(self.lab / "run.sh"), "main", self.meta["scenario"], *args],
                                env=dict(self.env, **env), capture_output=True, text=True, timeout=25)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        self.assertIn(reason, result.stderr.splitlines())
        return result

    def test_a_reply_at_eighty_percent_of_the_summary_timeout_is_a_behavioural_failure(self):
        self.run_lab(1, "lab: fail: no reply waited on a summary")

    def test_replies_under_the_threshold_pass_every_required_assertion(self):
        self.record(15999)
        self.run_lab(0, "lab: pass: every required assertion passed")

    def test_the_whole_run_deadline_terminates_a_sleeping_lab(self):
        self.run_lab(3, "lab: timeout: whole-run deadline of 0.5s exceeded", LAB_DEADLINE_SECS="0.5", STUB_SLEEP="1")

    def test_deleted_reports_are_missing_evidence(self):
        result = subprocess.run(["bash", str(self.lab / "run.sh"), "main", "summarizer-hang"],
                                env=dict(self.env, LAB_FAULT_DROP_REPORT="1"), capture_output=True, text=True)
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertEqual(result.stderr.splitlines()[-1], "lab: missing-evidence: required evidence absent or unparseable")

    def test_a_crashing_report_is_missing_evidence(self):
        self.run_lab(4, "lab: missing-evidence: report.py failed", STUB_REPORT_FAIL="1")

    def test_a_failed_build_is_infrastructure(self):
        self.run_lab(5, "lab: infrastructure: runner exited 5", STUB_CARGO_FAIL="1")

    def test_a_failed_collector_is_infrastructure(self):
        self.run_lab(5, "lab: infrastructure: runner exited 5", STUB_DOCKER_FAIL="1")

    def test_a_lab_exception_is_infrastructure_even_when_it_returns_one(self):
        self.run_lab(5, "lab: infrastructure: runner exited 5", STUB_LAB_FAIL="1")

    def test_exploration_is_never_reported_as_a_pass(self):
        self.meta["scenario"] = "summaries-off-vs-on"
        self.record(1)
        result = self.run_lab(0, "lab: exploratory: exploratory: not a regression assertion")
        self.assertIn("exploratory: not a regression assertion", result.stdout.splitlines())
        verdict = json.loads(next((self.lab / "runs").glob("*/verdict.json")).read_text())
        self.assertEqual(verdict["outcome"], "exploratory")
        self.assertFalse(any(a["required"] for a in verdict["assertions"]))

    def test_diagnostic_collection_does_not_enforce_a_behavioural_failure(self):
        result = self.run_lab(0, "lab: fail: no reply waited on a summary", "--diagnostic")
        self.assertIn("diagnostic: verdict not enforced", result.stdout.splitlines())

    def test_no_turns_are_missing_evidence(self):
        (self.recorded / "latency.csv").write_text("kind,conversation,phase,wall_ms,summary_in_flight\n")
        self.run_lab(4, "lab: missing-evidence: report.py failed")

    def test_usage_errors_keep_code_two(self):
        self.run_lab(2, "lab: usage: invalid arguments", "--bogus")


if __name__ == "__main__":
    unittest.main(verbosity=2)
