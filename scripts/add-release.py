#!/usr/bin/env python3
"""Adds a release to releases.json.

Run by .github/workflows/release.yml on tag push. A running nudo fetches that
manifest to tell its operator a newer version exists and to render the notes in
the dashboard, so this is the step that makes a release visible to anyone
already running one.

Deliberately a small script with no dependencies rather than a few lines of
inline YAML: it has to be idempotent (re-running a workflow must not duplicate
an entry), it has to preserve the `$comment` block that documents the file, and
it is worth being able to run it locally to see what would be written.

    scripts/add-release.py --version 0.2.0 \\
        --url https://github.com/loa212/nudo/releases/tag/v0.2.0 \\
        --notes-file notes.md

Its own tests:

    python3 -m unittest scripts.add_release_test
"""

from __future__ import annotations

import argparse
import datetime
import json
import pathlib
import re
import sys

# Kept in step with `compare_versions` in crates/server/src/updates.rs. Anything
# this rejects would sort as unknown there, so it is rejected here instead —
# at publish time, where a human can still fix it.
VERSION = re.compile(r"^\d+(\.\d+)*(-[0-9A-Za-z.-]+)?$")


def normalise_version(version: str) -> str:
    """Strips a leading `v` and validates the rest.

    The manifest stores versions without the prefix; the tag has one. Storing
    both forms would mean every consumer has to strip it.
    """
    version = version.strip().lstrip("v")
    if not VERSION.match(version):
        raise ValueError(
            f"{version!r} is not a version this manifest can sort. "
            "Expected something like 1.2.3 or 1.2.3-rc1."
        )
    return version


def version_key(version: str) -> tuple:
    """Sorts newest first, with a prerelease ordering below its release.

    Mirrors `compare_versions` in the server: 1.2.0 is newer than 1.2.0-rc1.
    """
    version = version.lstrip("v")
    base, _, pre = version.partition("-")
    numbers = tuple(int(part) for part in base.split(".") if part.isdigit())
    # A release sorts above a prerelease of the same numbers, so the flag is 1
    # for a release and 0 for a prerelease.
    return (numbers, 1 if not pre else 0, pre)


def add_release(
    manifest: dict,
    version: str,
    url: str,
    notes: str,
    published_at: str,
    breaking: bool,
) -> dict:
    """Returns the manifest with this release added or updated.

    Idempotent: publishing the same version twice replaces the entry rather
    than adding a second one, so re-running a failed workflow is safe.
    """
    version = normalise_version(version)
    entry = {
        "version": version,
        "published_at": published_at,
        "url": url,
        "breaking": breaking,
        "notes": notes.strip(),
    }

    releases = [
        release
        for release in manifest.get("releases", [])
        if release.get("version", "").lstrip("v") != version
    ]
    releases.append(entry)
    releases.sort(key=lambda release: version_key(release.get("version", "0")), reverse=True)

    updated = dict(manifest)
    updated["releases"] = releases
    return updated


def looks_breaking(notes: str) -> bool:
    """Whether the notes announce manual steps.

    The dashboard styles a breaking release differently and says so above the
    "upgrade" link, so getting this wrong in either direction matters. It errs
    towards flagging: a release wrongly marked as needing manual steps costs
    someone a careful read, while one wrongly marked as safe costs them an
    outage.
    """
    lowered = notes.lower()
    return any(
        phrase in lowered
        for phrase in (
            "breaking",
            "manual step",
            "manual action",
            "requires migration",
            "action required",
        )
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="the release version, with or without a leading v")
    parser.add_argument("--url", required=True, help="where to read the full notes")
    parser.add_argument("--notes-file", help="file holding the release notes; stdin if absent")
    parser.add_argument("--manifest", default="releases.json", type=pathlib.Path)
    parser.add_argument("--published-at", help="ISO date; today if absent")
    parser.add_argument(
        "--breaking",
        action="store_true",
        help="force the breaking flag on; otherwise it is inferred from the notes",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the manifest that would be written and change nothing",
    )
    args = parser.parse_args(argv)

    notes = (
        pathlib.Path(args.notes_file).read_text(encoding="utf-8")
        if args.notes_file
        else sys.stdin.read()
    )

    manifest = json.loads(args.manifest.read_text(encoding="utf-8")) if args.manifest.exists() else {}

    published_at = args.published_at or datetime.date.today().isoformat()
    updated = add_release(
        manifest,
        version=args.version,
        url=args.url,
        notes=notes,
        published_at=published_at,
        breaking=args.breaking or looks_breaking(notes),
    )

    rendered = json.dumps(updated, indent=2, ensure_ascii=False) + "\n"
    if args.dry_run:
        sys.stdout.write(rendered)
        return 0

    args.manifest.write_text(rendered, encoding="utf-8")
    print(f"{args.manifest}: recorded {normalise_version(args.version)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
