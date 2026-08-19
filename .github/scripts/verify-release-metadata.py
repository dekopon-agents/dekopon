#!/usr/bin/env python3
"""Validate shared workspace versions and the crates.io publication plan."""

import argparse
import json
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--plan", required=True, type=Path)
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

    versions = {package["version"] for package in workspace.values()}
    if len(versions) != 1:
        details = ", ".join(
            f"{name}={package['version']}" for name, package in sorted(workspace.items())
        )
        raise SystemExit(f"workspace packages do not share one version: {details}")
    version = versions.pop()

    if args.tag is not None and args.tag != f"v{version}":
        raise SystemExit(f"release tag {args.tag!r} does not match workspace version v{version}")

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
