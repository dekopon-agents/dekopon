#!/usr/bin/env python3
"""Exercise the actual CI Bash gates with controlled cargo tree output."""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
GATES = {
    "Verify gateway excludes privileged broker machinery": (
        "dekopond", ("broker", "broker-host", "brokerd", "http-host", "storage-host", "policy")
    ),
    "Verify broker excludes unprivileged orchestration": (
        "dekopon-brokerd", ("agent", "shell", "model", "process", "config")
    ),
}


def gate_body(name):
    workflow = (ROOT / ".github/workflows/ci.yml").read_text()
    marker = f"      - name: {name}\n"
    if workflow.count(marker) != 1:
        raise AssertionError(f"expected exactly one CI gate: {name}")
    step = workflow.split(marker, 1)[1].split("      - name:", 1)[0]
    return textwrap.dedent(step.split("        run: |\n", 1)[1])


class DaemonDependencyGates(unittest.TestCase):
    def test_actual_ci_bodies(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            cargo.write_text(
                '#!/bin/sh\n'
                '[ "$*" = "tree --locked -p $EXPECTED_PACKAGE --edges normal --prefix none" ] || exit 91\n'
                'printf "%s\\n" "$TREE_OUTPUT"\n'
                'exit "$CARGO_STATUS"\n'
            )
            cargo.chmod(0o755)
            for name, (package, forbidden) in GATES.items():
                body = gate_body(name)
                allowed = ["dekopon-broker-protocol", "serde", package]
                # Prefixes and suffixes must not be mistaken for exact package names.
                allowed += [f"dekopon-{item}-extra" for item in forbidden]
                allowed += [f"other-dekopon-{item}" for item in forbidden]
                clean = "\n".join(f"{item} v0.12.0" for item in allowed)
                controls = [("allowed", clean, 0, False)]
                controls += [
                    (item, clean + f"\ndekopon-{item} v0.12.0 (/source) (*)", 0, True)
                    for item in forbidden
                ]
                controls += [("cargo-error", clean, 42, True)]
                for label, tree, status, rejected in controls:
                    with self.subTest(gate=name, control=label):
                        result = subprocess.run(
                            ["bash", "-c", body],
                            env={
                                **os.environ,
                                "PATH": directory + os.pathsep + os.environ["PATH"],
                                "EXPECTED_PACKAGE": package,
                                "TREE_OUTPUT": tree,
                                "CARGO_STATUS": str(status),
                            },
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        self.assertEqual(result.returncode != 0, rejected, result.stderr)
                        if rejected and status == 0:
                            self.assertEqual(result.returncode, 1)
                            self.assertIn(f"dekopon-{label} v", result.stderr)
                        elif status:
                            self.assertEqual(result.returncode, status)


if __name__ == "__main__":
    unittest.main()
