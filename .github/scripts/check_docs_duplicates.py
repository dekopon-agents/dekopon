#!/usr/bin/env python3
"""Reject duplicated keyed bullets and rows inside Markdown sections and tables."""

from __future__ import annotations

import argparse
import re
from pathlib import Path

BULLET_KEY = re.compile(r"^\s*[-*]\s+(?:\*\*(?P<bold>.+?)\*\*|`(?P<code>[^`]+)`)(?:[.:]|\s|$)")
HEADING = re.compile(r"^#{1,6}\s+")
TABLE_SEPARATOR = re.compile(r"^:?-{3,}:?$")


def normalize(value: str) -> str:
    return " ".join(value.casefold().split()).rstrip(".:")


def markdown_files(paths: list[Path]) -> list[Path]:
    files: set[Path] = set()
    for path in paths:
        if path.is_dir():
            files.update(path.rglob("*.md"))
        elif path.suffix.casefold() == ".md":
            files.add(path)
    return sorted(files)


def duplicate_findings(path: Path) -> list[str]:
    findings: list[str] = []
    bullet_keys: dict[str, int] = {}
    table_keys: dict[str, int] = {}
    table_active = False
    in_fence = False

    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        stripped = line.strip()

        if stripped.startswith(("```", "~~~")):
            in_fence = not in_fence
            continue
        if in_fence:
            continue

        if HEADING.match(stripped):
            bullet_keys.clear()
            table_keys.clear()
            table_active = False
            continue

        bullet = BULLET_KEY.match(line)
        if bullet:
            key = normalize(bullet.group("bold") or bullet.group("code"))
            if key in bullet_keys:
                findings.append(
                    f"{path}:{line_number}: duplicate list key {key!r}; "
                    f"first used on line {bullet_keys[key]}"
                )
            else:
                bullet_keys[key] = line_number

        if stripped.startswith("|") and stripped.endswith("|"):
            cells = [cell.strip() for cell in stripped[1:-1].split("|")]
            first_cell = cells[0] if cells else ""
            if first_cell and not TABLE_SEPARATOR.fullmatch(first_cell):
                key = normalize(re.sub(r"[`*_]", "", first_cell))
                if table_active and key in table_keys:
                    findings.append(
                        f"{path}:{line_number}: duplicate table key {key!r}; "
                        f"first used on line {table_keys[key]}"
                    )
                else:
                    table_keys[key] = line_number
            table_active = True
        elif table_active:
            table_keys.clear()
            table_active = False

    return findings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=Path)
    args = parser.parse_args()

    files = markdown_files(args.paths)
    if not files:
        parser.error("no Markdown files found")

    findings = [finding for path in files for finding in duplicate_findings(path)]
    if findings:
        print(f"Documentation duplicate check failed ({len(findings)}):")
        for finding in findings:
            print(f"  - {finding}")
        return 1

    print(f"Checked {len(files)} Markdown files for duplicate keyed bullets and table rows.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
