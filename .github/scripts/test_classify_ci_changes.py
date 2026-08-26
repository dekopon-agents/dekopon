#!/usr/bin/env python3

import unittest

from classify_ci_changes import CATEGORIES, classify


class ClassifyCiChangesTests(unittest.TestCase):
    def assert_selected(self, paths: list[str], *selected: str) -> None:
        result = classify(paths)
        expected = {category: category in selected for category in CATEGORIES}
        self.assertEqual(result, expected)

    def test_prose_only_selects_no_expensive_lane(self) -> None:
        self.assert_selected(["docs/design.md", "README.md"])

    def test_workspace_source_preserves_transitive_cli_install_coverage(self) -> None:
        self.assert_selected(
            ["crates/dekopon-policy/src/lib.rs"],
            "run_rust",
            "run_otel",
            "run_cli_install",
        )

    def test_direct_binary_source_keeps_path_installation_coverage(self) -> None:
        self.assert_selected(
            ["crates/dekopon-run/src/main.rs"],
            "run_rust",
            "run_otel",
            "run_cli_install",
        )

    def test_lockfile_change_runs_every_rust_metadata_lane(self) -> None:
        self.assert_selected(
            ["Cargo.lock"],
            "run_rust",
            "run_otel",
            "run_package",
            "run_cli_install",
            "run_dependencies",
        )

    def test_crate_manifest_runs_package_and_dependency_checks(self) -> None:
        self.assert_selected(
            ["crates/dekopon-core/Cargo.toml"],
            "run_rust",
            "run_otel",
            "run_package",
            "run_cli_install",
            "run_dependencies",
        )

    def test_changelog_runs_only_release_and_chart_checks(self) -> None:
        self.assert_selected(
            ["CHANGELOG.md"],
            "run_release_metadata",
            "run_chart",
        )

    def test_chart_only_change_does_not_compile_rust(self) -> None:
        self.assert_selected(["charts/dekopon/templates/configmap.yaml"], "run_chart")

    def test_chart_shell_change_selects_its_own_shellcheck_lane(self) -> None:
        self.assert_selected(["charts/dekopon/ci/verify-init-permissions.sh"], "run_chart")

    def test_dependency_policy_change_runs_only_dependency_check(self) -> None:
        self.assert_selected(["deny.toml"], "run_dependencies")

    def test_package_cache_helper_selects_package_validation(self) -> None:
        self.assert_selected([".github/scripts/prepare-package-cache.sh"], "run_package")

    def test_load_bearing_examples_and_provider_helper_run_rust(self) -> None:
        for path in (
            "examples/conditional-write/broker.yaml",
            "examples/slack/manifest-agent.yaml",
            "examples/providers/build-component.sh",
        ):
            with self.subTest(path=path):
                self.assert_selected(
                    [path],
                    "run_rust",
                    "run_otel",
                    "run_cli_install",
                )

    def test_otel_fixture_preserves_rust_and_smoke_coverage(self) -> None:
        self.assert_selected(
            ["examples/otel-traces/smoke-test.sh"],
            "run_rust",
            "run_otel",
            "run_cli_install",
        )

    def test_ci_control_changes_fail_open_to_every_lane(self) -> None:
        self.assert_selected([".github/workflows/ci.yml"], *CATEGORIES)
        self.assert_selected([".github/scripts/ci_metrics.sh"], *CATEGORIES)

    def test_categories_union_across_paths(self) -> None:
        self.assert_selected(
            ["crates/dekopon-policy/src/lib.rs", "charts/dekopon/values.yaml"],
            "run_rust",
            "run_otel",
            "run_cli_install",
            "run_chart",
        )


if __name__ == "__main__":
    unittest.main()
