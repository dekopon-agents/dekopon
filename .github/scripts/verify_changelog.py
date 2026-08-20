#!/usr/bin/env python3
"""Validate one dated, non-empty Keep a Changelog release section."""

import argparse
import datetime
import re
from collections.abc import Collection
from pathlib import Path

CATEGORIES = ("Added", "Changed", "Deprecated", "Removed", "Fixed", "Security")
PLACEHOLDERS = {
    "add changes here",
    "n/a",
    "none",
    "nothing yet",
    "tbd",
    "todo",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--changelog", required=True, type=Path)
    parser.add_argument(
        "--entry",
        required=True,
        help="Heading identifier without brackets, for example 0.7.0 or dekopon-chart-0.1.1",
    )
    return parser.parse_args()


def _has_real_bullet(section: str) -> bool:
    category_pattern = re.compile(
        rf"^### (?:{'|'.join(CATEGORIES)})[ \t]*$", re.MULTILINE
    )
    subheading_pattern = re.compile(r"^### ", re.MULTILINE)

    for category in category_pattern.finditer(section):
        remainder = section[category.end() :]
        next_subheading = subheading_pattern.search(remainder)
        category_body = (
            remainder[: next_subheading.start()] if next_subheading else remainder
        )
        for line in category_body.splitlines():
            if not line.startswith("- "):
                continue
            item = line[2:].strip()
            normalized = item.casefold().strip(" .!`*_")
            if item and not item.startswith("<!--") and normalized not in PLACEHOLDERS:
                return True
    return False


def verify_changelog(
    path: Path,
    entry: str,
    *,
    tag: str | None = None,
    allow_missing_tags: Collection[str] = (),
) -> None:
    """Require an Unreleased heading and one dated, non-placeholder release entry."""

    if not path.is_file():
        if tag is not None and tag in allow_missing_tags:
            print(f"changelog: {tag} predates the enforced {path} history")
            return
        raise SystemExit(f"required changelog {path} does not exist")

    text = path.read_text(encoding="utf-8")
    unreleased = re.findall(r"^## \[Unreleased\][ \t]*$", text, re.MULTILINE)
    if len(unreleased) != 1:
        raise SystemExit(
            f"{path} must contain exactly one '## [Unreleased]' heading; "
            f"found {len(unreleased)}"
        )

    candidate_pattern = re.compile(
        rf"^## \[{re.escape(entry)}\](?P<suffix>[^\n]*)$", re.MULTILINE
    )
    candidates = list(candidate_pattern.finditer(text))
    if len(candidates) != 1:
        raise SystemExit(
            f"{path} must contain exactly one release heading for [{entry}]; "
            f"found {len(candidates)}"
        )

    candidate = candidates[0]
    dated_suffix = re.fullmatch(r" - (\d{4}-\d{2}-\d{2})", candidate["suffix"])
    if dated_suffix is None:
        raise SystemExit(
            f"{path} heading for [{entry}] must be exactly "
            f"'## [{entry}] - YYYY-MM-DD'"
        )

    release_date = dated_suffix.group(1)
    try:
        parsed_date = datetime.date.fromisoformat(release_date)
    except ValueError as error:
        raise SystemExit(
            f"{path} heading for [{entry}] has invalid date {release_date!r}"
        ) from error
    if parsed_date.isoformat() != release_date:
        raise SystemExit(
            f"{path} heading for [{entry}] has non-canonical date {release_date!r}"
        )

    remainder = text[candidate.end() :]
    next_release = re.search(r"^## ", remainder, re.MULTILINE)
    section = remainder[: next_release.start()] if next_release else remainder
    if not _has_real_bullet(section):
        categories = ", ".join(CATEGORIES)
        raise SystemExit(
            f"{path} release [{entry}] must contain a non-placeholder bullet under one of: "
            f"{categories}"
        )

    print(f"changelog entry: {entry} ({release_date})")


def main() -> None:
    args = parse_args()
    verify_changelog(args.changelog, args.entry)


if __name__ == "__main__":
    main()
