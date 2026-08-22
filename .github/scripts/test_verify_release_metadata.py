#!/usr/bin/env python3
"""Tests for verify-release-metadata.py using only the Python standard library."""

import importlib.util
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "verify_release_metadata",
    Path(__file__).with_name("verify-release-metadata.py"),
)
assert _SPEC is not None and _SPEC.loader is not None
verify_release_metadata = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(verify_release_metadata)

verify_libraries_are_consumed = verify_release_metadata.verify_libraries_are_consumed
GUEST_BINDING_PACKAGES = verify_release_metadata.GUEST_BINDING_PACKAGES


def package(name: str, *, binary: bool = False, dependencies: tuple[str, ...] = ()) -> dict:
    kind = ["bin"] if binary else ["lib"]
    return {
        "name": name,
        "targets": [{"kind": kind, "name": name}],
        "dependencies": [{"name": dependency} for dependency in dependencies],
    }


def workspace(*packages: dict) -> dict[str, dict]:
    return {entry["name"]: entry for entry in packages}


class VerifyLibrariesAreConsumedTests(unittest.TestCase):
    def exempt(self) -> list[dict]:
        return [package(name) for name in sorted(GUEST_BINDING_PACKAGES)]

    def test_accepts_a_library_a_binary_depends_on(self) -> None:
        verify_libraries_are_consumed(
            workspace(
                package("cli", binary=True, dependencies=("core",)),
                package("core"),
                *self.exempt(),
            )
        )

    def test_rejects_a_library_nothing_depends_on(self) -> None:
        with self.assertRaisesRegex(SystemExit, "testkit"):
            verify_libraries_are_consumed(
                workspace(
                    package("cli", binary=True, dependencies=("core",)),
                    package("core"),
                    package("testkit"),
                    *self.exempt(),
                )
            )

    def test_a_dev_dependency_is_a_consumer(self) -> None:
        # A crate only the test build of another member uses is still reachable code.
        verify_libraries_are_consumed(
            workspace(
                package("cli", binary=True, dependencies=("fixtures",)),
                package("fixtures"),
                *self.exempt(),
            )
        )

    def test_ignores_an_unconsumed_binary_member(self) -> None:
        verify_libraries_are_consumed(
            workspace(package("cli", binary=True), *self.exempt())
        )

    def test_a_member_depending_only_on_itself_is_not_consumed(self) -> None:
        with self.assertRaisesRegex(SystemExit, "core"):
            verify_libraries_are_consumed(
                workspace(
                    package("cli", binary=True),
                    package("core", dependencies=("core",)),
                    *self.exempt(),
                )
            )

    def test_rejects_an_exemption_for_a_departed_package(self) -> None:
        with self.assertRaisesRegex(SystemExit, "obsolete"):
            verify_libraries_are_consumed(
                workspace(package("cli", binary=True, dependencies=("core",)), package("core"))
            )

    def test_rejects_an_exemption_that_now_has_a_consumer(self) -> None:
        exempted = sorted(GUEST_BINDING_PACKAGES)[0]
        with self.assertRaisesRegex(SystemExit, exempted):
            verify_libraries_are_consumed(
                workspace(
                    package("cli", binary=True, dependencies=(exempted,)),
                    *self.exempt(),
                )
            )


if __name__ == "__main__":
    unittest.main()
