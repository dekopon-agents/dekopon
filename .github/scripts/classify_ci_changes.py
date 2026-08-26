#!/usr/bin/env python3
"""Classify changed paths into the smallest safe CI lanes.

The workflow treats a missing output as "run", so this script may optimize only
successful classifications. Keep every category conservative: an unnecessary
job is cheaper than silently losing coverage.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

CATEGORIES = (
    "run_rust",
    "run_otel",
    "run_package",
    "run_cli_install",
    "run_dependencies",
    "run_release_metadata",
    "run_chart",
)

FULL_CI_INPUTS = {
    ".github/workflows/ci.yml",
    ".github/workflows/cache-warm.yml",
    ".github/scripts/classify_ci_changes.py",
    ".github/scripts/test_classify_ci_changes.py",
    ".github/scripts/ci_metrics.sh",
}

RUST_ROOT_INPUTS = {
    ".clippy.toml",
    ".rustfmt.toml",
    "Cargo.lock",
    "Cargo.toml",
    "clippy.toml",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "examples/local/dekopon.yaml",
    "ci/fetch-external-provider-components.sh",
    "wit/http/http.wit",
    "wit/storage/storage.wit",
}

PACKAGE_ROOT_INPUTS = {
    ".github/release-crates.txt",
    ".github/scripts/prepare-package-cache.sh",
    ".github/scripts/test_verify_release_metadata.py",
    ".github/scripts/verify-release-metadata.py",
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
}

RELEASE_METADATA_INPUTS = {
    ".github/release-crates.txt",
    ".github/scripts/test_verify_changelog.py",
    ".github/scripts/test_verify_release_metadata.py",
    ".github/scripts/verify-release-metadata.py",
    ".github/scripts/verify_changelog.py",
    "CHANGELOG.md",
    "Cargo.toml",
}

CRATE_INPUT = re.compile(
    r"^crates/([^/]+)/(Cargo\.toml|build\.rs|(?:src|tests|examples|benches|wit)/.+|"
    r"(?:\.clippy|clippy|\.rustfmt|rustfmt)\.toml)$"
)
PROVIDER_INPUT = re.compile(
    r"^examples/providers/[^/]+/(Cargo\.lock|Cargo\.toml|build\.rs|"
    r"(?:src|tests|examples|benches|wit)/.+|"
    r"(?:\.clippy|clippy|\.rustfmt|rustfmt)\.toml)$"
)
PACKAGE_INPUT = re.compile(r"^crates/[^/]+/(Cargo\.toml|build\.rs|README\.md|wit/.+)$")


def _empty() -> dict[str, bool]:
    return {category: False for category in CATEGORIES}


def classify_path(path: str) -> dict[str, bool]:
    flags = _empty()

    if path in FULL_CI_INPUTS:
        return {category: True for category in CATEGORIES}

    if path.startswith(".cargo/"):
        flags.update(
            run_rust=True,
            run_package=True,
            run_cli_install=True,
            run_dependencies=True,
        )

    if path in RUST_ROOT_INPUTS:
        flags["run_rust"] = True

    crate = CRATE_INPUT.fullmatch(path)
    if crate:
        flags["run_rust"] = True

    if PROVIDER_INPUT.fullmatch(path):
        flags["run_rust"] = True

    if re.fullmatch(r"examples/pr-summarizer-linter/[^/]+\.(?:cedar|yaml|yaml\.example)", path):
        flags["run_rust"] = True

    if re.fullmatch(r"examples/providers/[^/]+-provider\.wasm", path):
        flags["run_rust"] = True

    if path.startswith("examples/otel-traces/"):
        flags.update(run_rust=True, run_otel=True)

    if path in PACKAGE_ROOT_INPUTS or PACKAGE_INPUT.fullmatch(path):
        flags["run_package"] = True

    if path in {"Cargo.lock", "Cargo.toml", "rust-toolchain.toml"} or path.startswith(
        ".cargo/"
    ):
        flags["run_cli_install"] = True

    if path in {"Cargo.lock", "Cargo.toml", "deny.toml", "rust-toolchain.toml"} or path.startswith(
        ".cargo/"
    ):
        flags["run_dependencies"] = True

    if crate and crate.group(2) in {"Cargo.toml", "build.rs"}:
        flags["run_dependencies"] = True
    if PROVIDER_INPUT.fullmatch(path) and path.endswith(("Cargo.lock", "Cargo.toml", "build.rs")):
        flags["run_dependencies"] = True

    if path in RELEASE_METADATA_INPUTS:
        flags["run_release_metadata"] = True

    if (
        path == "CHANGELOG.md"
        or path.startswith("charts/dekopon/")
        or path in {
            ".github/scripts/test_verify_changelog.py",
            ".github/scripts/verify_changelog.py",
        }
    ):
        flags["run_chart"] = True

    # Preserve the existing OTLP and release-profile install coverage for every
    # Rust-affecting change. Both exercise normal dependencies outside the four
    # binary crates, so path-only direct-crate gating would silently lose coverage.
    if flags["run_rust"]:
        flags["run_otel"] = True
        flags["run_cli_install"] = True

    return flags


def classify(paths: list[str]) -> dict[str, bool]:
    result = _empty()
    for path in paths:
        flags = classify_path(path)
        for category, selected in flags.items():
            result[category] = result[category] or selected
    return result


def _write_outputs(destination: Path, result: dict[str, bool]) -> None:
    with destination.open("a", encoding="utf-8") as output:
        for category in CATEGORIES:
            output.write(f"{category}={str(result[category]).lower()}\n")
        output.write(f"run_expensive={str(any(result.values())).lower()}\n")


def _write_summary(destination: Path, paths: list[str], result: dict[str, bool]) -> None:
    with destination.open("a", encoding="utf-8") as summary:
        summary.write("### CI path classification\n\n")
        summary.write("| Lane | Selected |\n|---|---:|\n")
        for category in CATEGORIES:
            summary.write(f"| `{category}` | `{str(result[category]).lower()}` |\n")
        summary.write("\nChanged paths considered: " + str(len(paths)) + ".\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--github-output", type=Path, required=True)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    raw_paths = sys.stdin.buffer.read().split(b"\0")
    paths = [raw.decode("utf-8") for raw in raw_paths if raw]
    result = classify(paths)

    for path in paths:
        selected = [name for name, enabled in classify_path(path).items() if enabled]
        if selected:
            print(f"CI input: {path} -> {', '.join(selected)}")

    _write_outputs(args.github_output, result)
    if args.summary:
        _write_summary(args.summary, paths, result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
