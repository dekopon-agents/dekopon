#!/usr/bin/env python3
"""Tests for render-homebrew-formula.py using only the Python standard library.

The formula this renders is published to the tap and is what `brew install` prints,
so the assertions read the rendered text and compare it against EXECUTABLES rather
than against a second copy of the binary list kept here.
"""

import contextlib
import hashlib
import importlib.util
import io
import re
import sys
import tempfile
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "render_homebrew_formula",
    Path(__file__).with_name("render-homebrew-formula.py"),
)
assert _SPEC is not None and _SPEC.loader is not None
render_homebrew_formula = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(render_homebrew_formula)

EXECUTABLES = render_homebrew_formula.EXECUTABLES
NUMBER_WORDS = render_homebrew_formula.NUMBER_WORDS

TAG = "v0.12.0"
VERSION = TAG[1:]
TARGETS = ("aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu")


def render() -> str:
    """Render a formula through the entry point the tap workflow invokes."""
    with tempfile.TemporaryDirectory() as workspace:
        checksums = Path(workspace) / "checksums"
        checksums.mkdir()
        for target in TARGETS:
            archive = f"dekopon-{VERSION}-{target}.tar.gz"
            digest = hashlib.sha256(archive.encode("utf-8")).hexdigest()
            (checksums / f"{archive}.sha256").write_text(
                f"{digest}  {archive}\n", encoding="utf-8"
            )
        output = Path(workspace) / "dekopon.rb"
        argv = [
            "render-homebrew-formula.py",
            "--repository",
            "dekopon-agents/dekopon",
            "--tag",
            TAG,
            "--checksums",
            str(checksums),
            "--output",
            str(output),
        ]
        original = sys.argv
        sys.argv = argv
        try:
            with contextlib.redirect_stderr(io.StringIO()):
                render_homebrew_formula.main()
        finally:
            sys.argv = original
        return output.read_text(encoding="utf-8")


class RenderHomebrewFormulaTests(unittest.TestCase):
    def setUp(self) -> None:
        self.formula = render()

    def test_install_stanza_names_every_executable_in_order(self) -> None:
        expected = ", ".join(f'"{name}"' for name, _ in EXECUTABLES)
        self.assertIn(f"bin.install {expected}\n", self.formula)

    def test_caveats_describe_every_executable_and_no_other(self) -> None:
        described = re.findall(
            r"^ {8}(\S+) {2,}(the .+)$", self.formula, flags=re.MULTILINE
        )
        self.assertEqual(described, [(name, text) for name, text in EXECUTABLES])

    def test_smoke_test_runs_every_installed_executable(self) -> None:
        exercised = re.findall(
            r'shell_output\("#\{bin\}/(\S+) --version"\)', self.formula
        )
        self.assertEqual(exercised, [name for name, _ in EXECUTABLES])
        for name, _ in EXECUTABLES:
            with self.subTest(executable=name):
                self.assertIn(f'assert_match "{name} #{{version}}"', self.formula)

    def test_every_executable_count_the_prose_states_matches_the_list(self) -> None:
        # The prose also says "every executable" where a count would age badly, so only
        # the spelled numbers are compared: any other one is a count that drifted.
        spelled = set(NUMBER_WORDS.values())
        counted = {
            word
            for word in re.findall(r"(\w+) executables?\b", self.formula)
            if word in spelled
        }
        self.assertEqual(counted, {NUMBER_WORDS[len(EXECUTABLES)]})

    def test_no_retired_binary_prose_survives(self) -> None:
        for stale in (
            "Three formulae",
            "all two executables",
            "four executables",
            "dekopon-run",
        ):
            with self.subTest(residue=stale):
                self.assertNotIn(stale, self.formula)


if __name__ == "__main__":
    unittest.main()
