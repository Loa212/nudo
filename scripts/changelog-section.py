#!/usr/bin/env python3
"""Extracts one version's section out of CHANGELOG.md.

Run by .github/workflows/release.yml to get the notes that go into
releases.json — which is what a running instance renders under "What's new".

Those notes are deliberately *not* the GitHub release body. The release page
carries install instructions, because that is where someone arrives to download
it; the dashboard's changelog is read by people already running nudo, for whom
install steps are noise.

    scripts/changelog-section.py 0.2.0 [--changelog CHANGELOG.md]

Its own tests:

    python3 scripts/changelog_section_test.py
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys


def extract(text: str, version: str) -> str:
    """Returns the body of this version's section, without its heading.

    Matches the heading git-cliff writes (`## 1.2.3 — 2026-07-27`), stopping at
    the next `## ` heading or the end. Returns an empty string when the version
    is absent, which the caller turns into a fallback rather than an error: a
    missing changelog section should not fail a release that is otherwise fine.
    """
    match = re.search(
        rf"^## {re.escape(version)}\b.*?$(.*?)(?=^## |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    if not match:
        return ""

    body = match.group(1)
    # git-cliff ends the file with a generator comment, which is not something
    # to show in a dashboard.
    body = re.sub(r"<!--.*?-->", "", body, flags=re.DOTALL)
    return body.strip()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="the version to extract, without a leading v")
    parser.add_argument("--changelog", default="CHANGELOG.md", type=pathlib.Path)
    args = parser.parse_args(argv)

    version = args.version.lstrip("v")
    text = args.changelog.read_text(encoding="utf-8") if args.changelog.exists() else ""

    section = extract(text, version)
    if not section:
        # Not fatal. The release still publishes and the dashboard still shows
        # the new version with a link — just without an inline summary.
        print(
            f"See the release page for what changed in {version}.",
            file=sys.stdout,
        )
        print(
            f"note: no section for {version} in {args.changelog}",
            file=sys.stderr,
        )
        return 0

    print(section)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
