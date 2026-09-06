#!/usr/bin/env python3
"""Print non-Rust consumers of a deletion inventory; any hit exits 1, errors exit 2.

Usage: python3 .github/scripts/check_simplify_residue.py --repo /worktree \
    --symbols /path/to/removed-symbols.txt

The inventory has one literal identifier, package/binary name, or path per line.
There is deliberately no allowlist: historical or future-unit hits remain visible.
"""

import argparse
import pathlib
import re
import subprocess
import sys


EXTENSIONS = {".md", ".yaml", ".yml", ".sh", ".py", ".toml"}


def scan(repo, symbols):
    """Return every (path, line, symbol, text) hit in tracked/unignored files."""
    result = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        check=True,
        stdout=subprocess.PIPE,
    )
    patterns = [
        (symbol, re.compile(r"(?<![A-Za-z0-9_-])" + re.escape(symbol) + r"(?![A-Za-z0-9_-])"))
        for symbol in symbols
    ]
    hits = []
    for name in sorted(set(result.stdout.decode().split("\0")) - {""}):
        path = pathlib.PurePosixPath(name)
        if "target" in path.parts:
            continue
        if path.suffix not in EXTENSIONS and ".github" not in path.parts:
            continue
        source = repo / path
        if source.is_symlink() or any(parent.is_symlink() for parent in source.parents if parent != repo):
            raise ValueError(f"refusing to follow a symlink while scanning {name}")
        if not source.exists():
            # An unstaged deletion is still listed by git ls-files --cached.
            continue
        for number, line in enumerate(source.read_text(encoding="utf-8").splitlines(), 1):
            for symbol, pattern in patterns:
                if pattern.search(line):
                    hits.append((name, number, symbol, line))
    return hits


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=pathlib.Path, required=True)
    parser.add_argument("--symbols", type=pathlib.Path, required=True)
    args = parser.parse_args(argv)
    try:
        symbols = sorted(set(args.symbols.read_text(encoding="utf-8").splitlines()) - {""})
        if not symbols or any(symbol != symbol.strip() for symbol in symbols):
            raise ValueError("inventory must contain nonempty literal symbols without edge whitespace")
        repo = args.repo.resolve(strict=True)
        root = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--show-toplevel"],
            check=True, stdout=subprocess.PIPE, text=True,
        ).stdout.strip()
        if pathlib.Path(root).resolve() != repo:
            raise ValueError("--repo must name the Git worktree root")
        hits = scan(repo, symbols)
        for name, number, symbol, line in hits:
            print(f"{name}:{number}: [{symbol}] {line}")
        print(f"Scanned {len(symbols)} removed symbols; {len(hits)} residue hits.", file=sys.stderr)
        return int(bool(hits))
    except (OSError, UnicodeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Residue check failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
