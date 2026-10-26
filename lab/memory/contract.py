#!/usr/bin/env python3
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

try:
    import report
except Exception:
    print("lab: missing-evidence: report.py failed", file=sys.stderr)
    if "--diagnostic" in sys.argv[1:]:
        print("diagnostic: verdict not enforced")
        sys.exit(0)
    sys.exit(4)

CODES = {"pass": 0, "fail": 1, "usage": 2, "timeout": 3,
         "missing-evidence": 4, "infrastructure": 5, "not-applicable": 6, "exploratory": 0}


def verdict_exit(run):
    report.evidence(run)
    if not (run / "summary.md").read_text().strip():
        raise ValueError("empty summary.md")
    verdict = json.loads((run / "verdict.json").read_text())
    if verdict["label"] not in {"required", "exploratory"}:
        raise ValueError("invalid verdict label")
    outcome = verdict["outcome"]
    if outcome not in {"pass", "fail", "not-applicable", "exploratory"} or not verdict["reason"]:
        raise ValueError("invalid verdict outcome")
    assertions = verdict["assertions"]
    if not isinstance(assertions, list) or not assertions:
        raise ValueError("no assertions")
    for assertion in assertions:
        if (not assertion["name"] or assertion["result"] not in {"pass", "fail", "not-applicable"}
                or not isinstance(assertion["numbers"], dict) or type(assertion["required"]) is not bool):
            raise ValueError("invalid assertion")
    required = [a for a in assertions if a["required"]]
    expected = ("exploratory" if verdict["label"] == "exploratory" else
                "fail" if any(a["result"] == "fail" for a in required) else
                "not-applicable" if any(a["result"] == "not-applicable" for a in required) else "pass")
    if outcome != expected or (verdict["label"] == "required" and not required):
        raise ValueError("inconsistent verdict")
    return outcome, verdict["reason"]


def main(args):
    def interrupted(*_):
        raise KeyboardInterrupt()

    signal.signal(signal.SIGTERM, interrupted)
    diagnostic = "--diagnostic" in args
    if len(args) < 2 or args[1] not in report.LABELS:
        print("lab: usage: expected <ref> <scenario> [--profile fake|real|codex] [--diagnostic]", file=sys.stderr)
        return 2
    try:
        deadline = float(os.environ.get("LAB_DEADLINE_SECS", report.DEADLINES[args[1]]))
        if not math.isfinite(deadline) or deadline <= 0:
            raise ValueError()
    except ValueError:
        print("lab: usage: LAB_DEADLINE_SECS must be positive and finite", file=sys.stderr)
        return 2
    outcome, reason = "infrastructure", "runner failed"
    with tempfile.TemporaryDirectory(prefix="memory-lab-") as scratch:
        pointer = Path(scratch) / "run"
        env = dict(os.environ, LAB_SUPERVISED="1", LAB_RUN_POINTER=str(pointer))
        child = subprocess.Popen(["bash", str(Path(__file__).with_name("run.sh")), *args],
                                 env=env, start_new_session=True)
        try:
            status = child.wait(timeout=deadline)
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            if isinstance(error, KeyboardInterrupt):
                outcome, reason = "infrastructure", "runner interrupted"
            else:
                outcome, reason = "timeout", f"whole-run deadline of {deadline:g}s exceeded"
        else:
            if status == 2:
                outcome, reason = "usage", "invalid arguments"
            elif status == 4:
                outcome, reason = "missing-evidence", "report.py failed"
            elif status != 0:
                outcome, reason = "infrastructure", f"runner exited {status}"
            else:
                try:
                    outcome, reason = verdict_exit(Path(pointer.read_text().strip()))
                except (OSError, ValueError, KeyError, TypeError):
                    outcome, reason = "missing-evidence", "required evidence absent or unparseable"
        if pointer.exists():
            run = Path(pointer.read_text().strip())
            result = {"outcome": outcome, "exit_code": CODES[outcome], "reason": reason}
            (run / "exit.json").write_text(json.dumps(result, indent=2) + "\n")
    if outcome == "exploratory":
        print("exploratory: not a regression assertion")
    print(f"lab: {outcome}: {reason}", file=sys.stderr)
    if diagnostic:
        print("diagnostic: verdict not enforced")
        if outcome not in {"infrastructure", "usage"}:
            return 0
    return CODES[outcome]


if __name__ == "__main__":
    try:
        result = main(sys.argv[1:])
    except (Exception, KeyboardInterrupt) as error:
        print(f"lab: infrastructure: runner could not complete ({type(error).__name__})", file=sys.stderr)
        result = 5
    sys.exit(result)
