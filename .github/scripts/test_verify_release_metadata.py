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
is_stripped_dev_dependency = verify_release_metadata.is_stripped_dev_dependency


def package(
    name: str,
    *,
    binary: bool = False,
    dependencies: tuple[str, ...] = (),
    dev_dependencies: tuple[str, ...] = (),
) -> dict:
    kind = ["bin"] if binary else ["lib"]
    return {
        "name": name,
        "targets": [{"kind": kind, "name": name}],
        "dependencies": [
            *({"name": dependency} for dependency in dependencies),
            *({"name": dependency, "kind": "dev"} for dependency in dev_dependencies),
        ],
    }


def workspace(*packages: dict) -> dict[str, dict]:
    return {entry["name"]: entry for entry in packages}


class VerifyLibrariesAreConsumedTests(unittest.TestCase):
    def test_accepts_a_library_a_binary_depends_on(self) -> None:
        verify_libraries_are_consumed(
            workspace(package("cli", binary=True, dependencies=("core",)), package("core"))
        )

    def test_accepts_transitively_used_libraries(self) -> None:
        verify_libraries_are_consumed(
            workspace(
                package("cli", binary=True, dependencies=("bridge",)),
                package("bridge", dependencies=("core",)),
                package("core"),
            )
        )

    def test_rejects_a_library_nothing_depends_on(self) -> None:
        with self.assertRaisesRegex(SystemExit, "testkit"):
            verify_libraries_are_consumed(
                workspace(
                    package("cli", binary=True, dependencies=("core",)),
                    package("core"),
                    package("testkit"),
                )
            )

    def test_rejects_an_unconsumed_former_guest_binding(self) -> None:
        with self.assertRaisesRegex(SystemExit, "dekopon-provider-http"):
            verify_libraries_are_consumed(
                workspace(package("cli", binary=True), package("dekopon-provider-http"))
            )

    def test_a_dev_dependency_is_a_consumer(self) -> None:
        verify_libraries_are_consumed(
            workspace(
                package("cli", binary=True, dev_dependencies=("fixtures",)),
                package("fixtures"),
            )
        )

    def test_ignores_an_unconsumed_binary_member(self) -> None:
        verify_libraries_are_consumed(workspace(package("cli", binary=True)))

    def test_a_member_depending_only_on_itself_is_not_consumed(self) -> None:
        with self.assertRaisesRegex(SystemExit, "core"):
            verify_libraries_are_consumed(
                workspace(
                    package("cli", binary=True),
                    package("core", dependencies=("core",)),
                )
            )


class IsStrippedDevDependencyTests(unittest.TestCase):
    """A path-only dev-dependency is how a crate depends on a harness that depends back on it."""

    def test_a_path_only_dev_dependency_is_stripped(self) -> None:
        self.assertTrue(is_stripped_dev_dependency({"name": "testkit", "kind": "dev", "req": "*"}))

    def test_a_versioned_dev_dependency_survives_packaging(self) -> None:
        # This one reaches the published manifest, so it still constrains publication order.
        self.assertFalse(
            is_stripped_dev_dependency({"name": "sha2", "kind": "dev", "req": "^0.11.0"})
        )

    def test_a_normal_dependency_is_never_stripped(self) -> None:
        # `kind` is null for normal dependencies, and a path-only one still carries a version in
        # this workspace; neither may be skipped.
        self.assertFalse(is_stripped_dev_dependency({"name": "core", "kind": None, "req": "*"}))
        self.assertFalse(
            is_stripped_dev_dependency({"name": "core", "kind": None, "req": "^0.10.0"})
        )

    def test_a_build_dependency_is_never_stripped(self) -> None:
        self.assertFalse(is_stripped_dev_dependency({"name": "cc", "kind": "build", "req": "*"}))


if __name__ == "__main__":
    unittest.main()
