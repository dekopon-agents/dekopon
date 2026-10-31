import csv
import json
import os
import resource
from pathlib import Path
import shutil
import signal
import time
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
        case "$1" in
          run) touch "$STUB_ROOT/collector" ;;
          container) test -f "$STUB_ROOT/collector" ;;
          inspect)
            case "$3" in
              *Running*) if [ -n "${STUB_COLLECTOR_EARLY_EXIT:-}" ]; then echo false; else echo true; fi ;;
              *ExitCode*) echo "${STUB_COLLECTOR_EARLY_EXIT:-${STUB_COLLECTOR_EXIT:-0}}" ;;
            esac ;;
          rm) rm -f "$STUB_ROOT/collector" ;;
        esac''')
        self.stub("curl", "exit 0")
        self.stub("id", "echo 1000")
        self.stub("python3", '''case "$1" in
          */lab.py)
            shift
            while [ "$1" != --run ]; do shift; done
            run="$2"
            [ "${STUB_SLEEP:-0}" != 1 ] || { touch "$STUB_ROOT/sleeping"; sleep 60; }
            [ "${STUB_LAB_FAIL:-0}" != 1 ] || exit 1
            cp "$STUB_RECORDED/"* "$run/"
            ;;
          */report.py)
            [ "${STUB_REPORT_FAIL:-0}" != 1 ] || exit 19
            "$STUB_PYTHON" "$@"
            [ ! -f "$STUB_ROOT/verdict_fault.py" ] || "$STUB_PYTHON" "$STUB_ROOT/verdict_fault.py" "$2"
            ;;
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
            writer.writerow(["turn", "kind", "conversation", "phase", "wall_ms", "summary_in_flight"])
            for turn in range(1, 11):
                writer.writerow([turn, "reply", "summary", "run", wall, "yes"])
                writer.writerow([turn, "reply", "none", "run", 1, "no"])
        behaviour = self.meta["scenario"].removeprefix("summarizer-")
        statuses = {"hang": "hung-client-gone", "fail": "503", "refuse": "refused"}
        call = {"kind": "summary", "tag": {"turn": "summary-1", "sum": behaviour},
                "status": statuses.get(behaviour, "summary")}
        (self.recorded / "model-calls.jsonl").write_text(json.dumps(call) + "\n")
        (self.recorded / "traces.jsonl").write_text('{"resourceSpans": []}\n')

    def tamper_verdict(self, change):
        (self.root / "verdict_fault.py").write_text(
            "import json, sys\nfrom pathlib import Path\n"
            "path = Path(sys.argv[1]) / 'verdict.json'\n"
            "verdict = json.loads(path.read_text())\n" + change + "\n"
            "path.write_text(json.dumps(verdict))\n")

    def run_lab(self, code, reason, *args, memory_limit=None, **env):
        options = {}
        if memory_limit is not None:
            options["preexec_fn"] = lambda: resource.setrlimit(resource.RLIMIT_AS, (memory_limit, memory_limit))
        result = subprocess.run(["bash", str(self.lab / "run.sh"), "main", self.meta["scenario"], *args],
                                env=dict(self.env, **env), capture_output=True, text=True, timeout=25, **options)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        self.assertIn(reason, result.stderr.splitlines())
        return result

    def test_a_reply_at_eighty_percent_of_the_summary_timeout_is_a_behavioural_failure(self):
        self.run_lab(1, "lab: fail: no reply waited on a summary")

    def test_replies_under_the_threshold_pass_every_required_assertion(self):
        self.record(15999)
        self.run_lab(0, "lab: pass: every required assertion passed")

    def test_summarizer_scenarios_require_both_complete_scripted_routes(self):
        with (self.recorded / "latency.csv").open("w") as stream:
            stream.write("turn,kind,conversation,phase,wall_ms,summary_in_flight\n1,reply,none,run,1,no\n")
        self.run_lab(4, "lab: missing-evidence: report.py failed")

    def test_summarizer_scenarios_require_observed_scripted_summarizer_behavior(self):
        self.record(1)
        for calls in ("", json.dumps({"kind": "summary", "tag": {"sum": "ok", "turn": "summary-1"},
                                       "status": "summary"}) + "\n"):
            with self.subTest(calls=calls):
                (self.recorded / "model-calls.jsonl").write_text(calls)
                self.run_lab(4, "lab: missing-evidence: report.py failed")

    def test_explicit_summarizer_non_applicability_is_not_a_pass(self):
        self.meta["findings"] = {"summarizer applicability": {
            "result": "not-applicable", "reason": "tested ref has no configured summarizer"}}
        self.record(1)
        (self.recorded / "model-calls.jsonl").write_text("")
        self.run_lab(6, "lab: not-applicable: required assertion not applicable at aaaaaaaa")

    def test_a_verdict_cannot_downgrade_omit_or_replace_the_scenarios_assertions(self):
        faults = {
            "label": "verdict.update(label='exploratory', outcome='exploratory')",
            "omitted": "verdict['assertions'] = [a for a in verdict['assertions'] if a['name'] != 'no reply waited on a summary']; verdict['outcome'] = 'pass'",
            "unknown": "verdict['assertions'][0]['name'] = 'unknown assertion'",
            "downgraded": "verdict['assertions'][2]['required'] = False; verdict['outcome'] = 'pass'",
            "scenario": "verdict['scenario'] = 'baseline'",
            "duplicate": "verdict['assertions'][0] = verdict['assertions'][1]",
        }
        for name, change in faults.items():
            with self.subTest(fault=name):
                self.tamper_verdict(change)
                self.run_lab(4, "lab: missing-evidence: required evidence absent or unparseable")

    def test_the_verdict_scenario_must_match_the_requested_run(self):
        meta = dict(self.meta, scenario="baseline")
        (self.recorded / "meta.json").write_text(json.dumps(meta))
        self.run_lab(4, "lab: missing-evidence: required evidence absent or unparseable")

    def test_lab_import_exceptions_are_infrastructure_and_not_usage(self):
        (self.lab / "lab.py").write_text('raise RuntimeError("lab import failed")\n')
        self.run_lab(5, "lab: infrastructure: runner exited 5")

    def test_unknown_scenarios_remain_usage_errors(self):
        self.meta["scenario"] = "unknown"
        self.run_lab(2, "lab: usage: expected <ref> <scenario> [--profile fake|real|codex] [--diagnostic]")

    def test_a_collector_that_already_exited_cannot_pass_even_with_exit_zero(self):
        self.record(1)
        self.run_lab(5, "lab: infrastructure: collector exited unexpectedly (running=false, exit=0)",
                     STUB_COLLECTOR_EARLY_EXIT="0")

    def test_a_collectors_nonzero_terminal_status_is_infrastructure(self):
        self.record(1)
        self.run_lab(5, "lab: infrastructure: collector exited unexpectedly (running=true, exit=23)",
                     STUB_COLLECTOR_EXIT="23")

    def test_an_already_failed_collector_cannot_be_hidden_by_retained_evidence(self):
        self.record(1)
        self.run_lab(5, "lab: infrastructure: collector exited unexpectedly (running=false, exit=23)",
                     STUB_COLLECTOR_EARLY_EXIT="23")

    def test_trace_validation_and_reporting_stream_beyond_the_process_memory_budget(self):
        self.record(1)
        batch = {"resourceSpans": [{"resource": {"attributes": [
            {"key": "test.payload", "value": {"stringValue": "x" * 4096}}]}, "scopeSpans": []}]}
        block = (json.dumps(batch) + "\n") * 64
        with (self.recorded / "traces.jsonl").open("w") as stream:
            for _ in range(512):
                stream.write(block)
        self.run_lab(0, "lab: pass: every required assertion passed", memory_limit=96 * 1024 * 1024)

    def test_the_whole_run_deadline_terminates_a_sleeping_lab(self):
        self.run_lab(3, "lab: timeout: whole-run deadline of 0.5s exceeded", LAB_DEADLINE_SECS="0.5", STUB_SLEEP="1")

    def test_interrupting_a_run_stops_its_child_and_reports_infrastructure(self):
        child = subprocess.Popen(["bash", str(self.lab / "run.sh"), "main", "summarizer-hang"],
                                 env=dict(self.env, STUB_SLEEP="1"), stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: child.poll() is not None or child.kill())
        deadline = time.monotonic() + 10
        while not (self.root / "sleeping").exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertTrue((self.root / "sleeping").exists())
        child.send_signal(signal.SIGTERM)
        stdout, stderr = child.communicate(timeout=20)
        self.assertEqual(child.returncode, 5, stdout + stderr)
        self.assertEqual(stderr.splitlines()[-1], "lab: infrastructure: runner interrupted")

    def test_deleted_reports_are_missing_evidence(self):
        result = subprocess.run(["bash", str(self.lab / "run.sh"), "main", "summarizer-hang"],
                                env=dict(self.env, LAB_FAULT_DROP_REPORT="1"), capture_output=True, text=True)
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertEqual(result.stderr.splitlines()[-1], "lab: missing-evidence: required evidence absent or unparseable")

    def test_a_crashing_report_is_missing_evidence(self):
        self.run_lab(4, "lab: missing-evidence: report.py failed", STUB_REPORT_FAIL="1")

    def test_a_reporter_that_crashes_on_import_is_missing_evidence(self):
        (self.lab / "report.py").write_text('raise RuntimeError("report failed")\n')
        self.run_lab(4, "lab: missing-evidence: report.py failed")

    def test_a_missing_shell_is_infrastructure_and_never_behavioural(self):
        result = subprocess.run([self.env["STUB_PYTHON"], str(self.lab / "contract.py"),
                                 "main", "baseline"], env=dict(self.env, PATH=str(self.root / "absent")),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 5)
        self.assertEqual(result.stderr.strip(),
                         "lab: infrastructure: runner could not complete (FileNotFoundError)")

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

    def test_slot_resume_without_slot_evidence_is_exploratory(self):
        self.meta["scenario"] = "restart-mid-summary"
        self.record(1)
        self.run_lab(0, "lab: exploratory: exploratory: not a regression assertion")
        verdict = json.loads(next((self.lab / "runs").glob("*/verdict.json")).read_text())
        self.assertEqual(verdict["assertions"][-1]["result"], "not-applicable")

    def test_diagnostic_mode_is_recorded_in_the_human_summary(self):
        self.meta["diagnostic"] = True
        self.record(16000)
        self.run_lab(0, "lab: fail: no reply waited on a summary", "--diagnostic")
        summary = next((self.lab / "runs").glob("*/summary.md")).read_text()
        self.assertIn("diagnostic: verdict not enforced", summary.splitlines())

    def test_no_turns_are_missing_evidence(self):
        (self.recorded / "latency.csv").write_text("kind,conversation,phase,wall_ms,summary_in_flight\n")
        self.run_lab(4, "lab: missing-evidence: report.py failed")

    def test_usage_errors_keep_code_two(self):
        self.run_lab(2, "lab: usage: invalid arguments", "--bogus")


if __name__ == "__main__":
    unittest.main(verbosity=2)
