#!/usr/bin/env python3
"""Regression coverage for the simplification non-Rust completeness check."""

import pathlib
import subprocess
import tempfile
import unittest

from check_simplify_residue import main, scan


class ResidueTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = pathlib.Path(self.temp.name).resolve()
        subprocess.run(["git", "-C", str(self.repo), "init", "-q"], check=True)

    def put(self, path, text):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text, encoding="utf-8")
        return file

    def test_all_non_rust_surfaces_including_hidden_github_are_scanned(self):
        names = [f"docs/consumer{ext}" for ext in (".md", ".yaml", ".yml", ".sh", ".py", ".toml")]
        names += [".github/release-crates.txt", ".github/workflows/ci.yml"]
        for name in names:
            self.put(name, "dekopon-run\n")
        self.put("src/consumer.rs", "dekopon-run\n")
        self.assertEqual({hit[0] for hit in scan(self.repo, ["dekopon-run"])}, set(names))

    def test_target_is_excluded_even_if_tracked(self):
        self.put("nested/target/consumer.md", "RetiredApi\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "nested/target/consumer.md"], check=True)
        self.assertEqual(scan(self.repo, ["RetiredApi"]), [])

    def test_identifier_boundaries_do_not_match_other_crates(self):
        self.put("README.md", "dekopon-brokerd\ndekopon-broker\ncrates/retired/src/lib.rs\n")
        hits = scan(self.repo, ["dekopon-broker", "crates/retired/src/lib.rs"])
        self.assertEqual([(hit[1], hit[2]) for hit in hits], [(2, "dekopon-broker"), (3, "crates/retired/src/lib.rs")])

    def test_history_and_execution_documents_are_not_silently_exempted(self):
        for name in ("CHANGELOG.md", "SIMPLIFY.md", "SIMPLIFY-BRIEF.md"):
            self.put(name, "RetiredApi\n")
        self.assertEqual(len(scan(self.repo, ["RetiredApi"])), 3)

    def test_tracked_but_deleted_file_is_not_a_consumer(self):
        file = self.put("deleted.md", "RetiredApi\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "deleted.md"], check=True)
        file.unlink()
        self.assertEqual(scan(self.repo, ["RetiredApi"]), [])

    def test_symlink_is_not_followed(self):
        (self.repo / "external.md").symlink_to("/not-an-authorized-input")
        with self.assertRaisesRegex(ValueError, "symlink"):
            scan(self.repo, ["RetiredApi"])

    def test_exit_status_distinguishes_clean_residue_and_bad_inventory(self):
        inventory = self.put("removed.txt", "RetiredApi\n")
        args = ["--repo", str(self.repo), "--symbols", str(inventory)]
        self.assertEqual(main(args), 0)
        self.put("README.md", "RetiredApi\n")
        self.assertEqual(main(args), 1)
        inventory.write_text("", encoding="utf-8")
        self.assertEqual(main(args), 2)


if __name__ == "__main__":
    unittest.main()
