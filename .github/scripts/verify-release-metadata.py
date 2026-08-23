#!/usr/bin/env python3
"""Validate shared workspace versions, changelog, and crates.io publication plan."""

import argparse
import json
from pathlib import Path

from verify_changelog import verify_changelog


# These immutable tags predate CHANGELOG.md. Current automation is intentionally staged before a
# manual recovery checks out an older tag, so only this exact historical set may omit the file.
LEGACY_CHANGELOGLESS_TAGS = frozenset(
    {"v0.2.0", "v0.3.0", "v0.4.0", "v0.5.0", "v0.6.0", "v0.7.0"}
)

# cargo-machete finds a dependency nothing uses; it cannot see a whole crate nothing depends on.
# These two are the only members allowed to have no workspace consumer: they are guest bindings
# compiled into provider components, whose in-repository callers are the excluded
# examples/providers/* workspaces.
GUEST_BINDING_PACKAGES = frozenset({"dekopon-provider-http", "dekopon-provider-storage"})


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--plan", required=True, type=Path)
    parser.add_argument("--changelog", required=True, type=Path)
    parser.add_argument("--tag")
    return parser.parse_args()


def read_plan(path: Path) -> list[str]:
    configured: list[str] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        entry = line.strip()
        if not entry or entry.startswith("#"):
            continue
        if line != entry or any(character.isspace() for character in entry):
            raise SystemExit(f"{path}:{line_number}: expected exactly one crate name")
        configured.append(entry)

    if not configured:
        raise SystemExit(f"{path} contains no crate names")
    if len(configured) != len(set(configured)):
        raise SystemExit(f"{path} contains a duplicate crate name")
    return configured


def is_stripped_dev_dependency(dependency: dict) -> bool:
    """Reports whether `cargo package` removes this dependency from the published manifest.

    A dev-dependency carrying no version requirement is path-only, and Cargo drops it when it
    packages the crate. It therefore imposes no crates.io publication order — which matters
    because it is how a crate depends on a test harness that depends back on it. A dev-dependency
    that *does* carry a version survives packaging and must still be ordered.
    """

    return dependency.get("kind") == "dev" and dependency.get("req") == "*"


def verify_libraries_are_consumed(workspace: dict[str, dict]) -> None:
    """Require every non-binary member to be a dependency of some other member."""

    consumed: set[str] = set()
    for name, package in workspace.items():
        for dependency in package["dependencies"]:
            if dependency["name"] in workspace and dependency["name"] != name:
                consumed.add(dependency["name"])

    libraries = {
        name
        for name, package in workspace.items()
        if not any("bin" in target["kind"] for target in package["targets"])
    }
    dead = sorted(libraries - consumed - GUEST_BINDING_PACKAGES)
    if dead:
        raise SystemExit(
            "no workspace member depends on these library crates; give each a consumer "
            f"or delete it: {', '.join(dead)}"
        )

    stale = sorted(GUEST_BINDING_PACKAGES - (libraries - consumed))
    if stale:
        raise SystemExit(
            "these guest-binding exemptions are obsolete and must be removed from "
            f"GUEST_BINDING_PACKAGES: {', '.join(stale)}"
        )


def main() -> None:
    args = parse_args()
    metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    configured = read_plan(args.plan)

    members = set(metadata["workspace_members"])
    workspace = {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in members
    }

    verify_libraries_are_consumed(workspace)

    versions = {package["version"] for package in workspace.values()}
    if len(versions) != 1:
        details = ", ".join(
            f"{name}={package['version']}" for name, package in sorted(workspace.items())
        )
        raise SystemExit(f"workspace packages do not share one version: {details}")
    version = versions.pop()

    if args.tag is not None and args.tag != f"v{version}":
        raise SystemExit(f"release tag {args.tag!r} does not match workspace version v{version}")

    verify_changelog(
        args.changelog,
        version,
        tag=args.tag,
        allow_missing_tags=LEGACY_CHANGELOGLESS_TAGS,
    )

    publishable = {
        name: package
        for name, package in workspace.items()
        if package.get("publish") != []
    }
    missing = sorted(set(publishable) - set(configured))
    extra = sorted(set(configured) - set(publishable))
    if missing or extra:
        raise SystemExit(
            "publication plan does not match publishable workspace packages; "
            f"missing={missing}, extra={extra}"
        )

    position = {name: index for index, name in enumerate(configured)}
    for name, package in publishable.items():
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if is_stripped_dev_dependency(dependency):
                continue
            if dependency_name in publishable and position[dependency_name] > position[name]:
                raise SystemExit(
                    f"{dependency_name} must be published before dependent package {name}"
                )

    print(f"workspace version: {version}")
    print("crates.io publication order:")
    for index, name in enumerate(configured, start=1):
        print(f"{index:>2}. {name}")


if __name__ == "__main__":
    main()
