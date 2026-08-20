#!/usr/bin/env python3
"""Tests for verify_changelog.py using only the Python standard library."""

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from verify_changelog import verify_changelog


class VerifyChangelogTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.path = Path(self.temporary_directory.name) / "CHANGELOG.md"

    def write(self, release: str, *, unreleased: str = "## [Unreleased]") -> None:
        self.path.write_text(
            f"# Changelog\n\n{unreleased}\n\n{release}\n",
            encoding="utf-8",
        )

    def verify(
        self,
        entry: str = "1.2.3",
        *,
        tag: str | None = None,
        allow_missing_tags: tuple[str, ...] = (),
    ) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            verify_changelog(
                self.path,
                entry,
                tag=tag,
                allow_missing_tags=allow_missing_tags,
            )

    def test_accepts_dated_application_entry(self) -> None:
        self.write("## [1.2.3] - 2026-08-20\n\n### Added\n\n- Shipped it.")
        self.verify()

    def test_accepts_full_chart_tag_entry(self) -> None:
        self.write(
            "## [dekopon-chart-0.1.1] - 2026-08-20\n\n"
            "### Fixed\n\n- Corrected a template."
        )
        self.verify("dekopon-chart-0.1.1")

    def test_rejects_missing_file(self) -> None:
        with self.assertRaisesRegex(SystemExit, "does not exist"):
            self.verify()

    def test_allows_only_an_explicit_legacy_missing_tag(self) -> None:
        self.verify(tag="v0.6.0", allow_missing_tags=("v0.6.0",))
        with self.assertRaisesRegex(SystemExit, "does not exist"):
            self.verify(tag="v0.6.1", allow_missing_tags=("v0.6.0",))

    def test_requires_exactly_one_unreleased_heading(self) -> None:
        release = "## [1.2.3] - 2026-08-20\n\n### Added\n\n- Shipped it."
        self.write(release, unreleased="## Unreleased")
        with self.assertRaisesRegex(SystemExit, "exactly one.*Unreleased"):
            self.verify()

        self.write(release, unreleased="## [Unreleased]\n\n## [Unreleased]")
        with self.assertRaisesRegex(SystemExit, "found 2"):
            self.verify()

    def test_requires_exactly_one_matching_release_heading(self) -> None:
        self.write("## [1.2.2] - 2026-08-20\n\n### Added\n\n- Older.")
        with self.assertRaisesRegex(SystemExit, r"release heading for \[1\.2\.3\].*found 0"):
            self.verify()

        release = "## [1.2.3] - 2026-08-20\n\n### Added\n\n- Shipped it."
        self.write(f"{release}\n\n{release}")
        with self.assertRaisesRegex(SystemExit, "found 2"):
            self.verify()

    def test_requires_a_canonical_valid_date(self) -> None:
        self.write("## [1.2.3] (2026-08-20)\n\n### Added\n\n- Shipped it.")
        with self.assertRaisesRegex(SystemExit, "must be exactly"):
            self.verify()

        self.write("## [1.2.3] - 2026-02-30\n\n### Added\n\n- Shipped it.")
        with self.assertRaisesRegex(SystemExit, "invalid date"):
            self.verify()

    def test_requires_a_real_bullet_in_a_keep_a_changelog_category(self) -> None:
        self.write("## [1.2.3] - 2026-08-20\n\n### Added")
        with self.assertRaisesRegex(SystemExit, "non-placeholder bullet"):
            self.verify()

        self.write("## [1.2.3] - 2026-08-20\n\n### Added\n\n- TODO")
        with self.assertRaisesRegex(SystemExit, "non-placeholder bullet"):
            self.verify()

        self.write("## [1.2.3] - 2026-08-20\n\n### Notes\n\n- Shipped it.")
        with self.assertRaisesRegex(SystemExit, "non-placeholder bullet"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
