#!/usr/bin/env python3
"""Tests for add-release.py.

This script runs unattended on tag push, and the file it writes is what every
running instance reads to learn about a new version. A bug here is invisible
until someone's dashboard shows the wrong thing.

    python3 scripts/add_release_test.py
"""

from __future__ import annotations

import importlib.util
import pathlib
import tempfile
import unittest

# The script has a hyphen in its name, so it cannot be imported normally.
_spec = importlib.util.spec_from_file_location(
    "add_release", pathlib.Path(__file__).parent / "add-release.py"
)
add_release = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(add_release)


def manifest_with(*versions: str) -> dict:
    return {
        "$comment": ["kept"],
        "releases": [{"version": v, "notes": f"notes for {v}"} for v in versions],
    }


def add(
    manifest,
    version,
    notes="Some notes.",
    breaking=False,
    url="https://example.test",
    artifacts=None,
):
    return add_release.add_release(
        manifest,
        version=version,
        url=url,
        notes=notes,
        published_at="2026-07-26",
        breaking=breaking,
        artifacts=artifacts,
    )

# A well-formed digest for tests; the value is arbitrary.
DIGEST = "a" * 64


class Versions(unittest.TestCase):
    def test_the_tags_leading_v_is_dropped(self):
        # The tag is v1.2.3; the manifest stores 1.2.3. Storing both forms would
        # make every consumer strip it.
        self.assertEqual(add_release.normalise_version("v1.2.3"), "1.2.3")
        self.assertEqual(add_release.normalise_version("1.2.3"), "1.2.3")

    def test_a_version_the_server_could_not_sort_is_rejected(self):
        # Better to fail the release workflow than to publish an entry that
        # every running instance silently ignores.
        for bad in ["not-a-version", "1.2.x", "", "v"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                add_release.normalise_version(bad)

    def test_a_prerelease_sorts_below_its_release(self):
        self.assertLess(
            add_release.version_key("1.2.0-rc1"),
            add_release.version_key("1.2.0"),
        )

    def test_versions_sort_numerically_not_as_text(self):
        # The bug this catches: "1.10.0" < "1.9.0" as strings, which would show
        # everyone running 1.10.0 that 1.9.0 is newer.
        self.assertGreater(
            add_release.version_key("1.10.0"),
            add_release.version_key("1.9.0"),
        )


class AddingARelease(unittest.TestCase):
    def test_the_newest_release_ends_up_first(self):
        result = add(manifest_with("1.0.0", "0.9.0"), "1.1.0")
        self.assertEqual(
            [r["version"] for r in result["releases"]],
            ["1.1.0", "1.0.0", "0.9.0"],
        )

    def test_an_out_of_order_tag_still_lands_in_the_right_place(self):
        # Tagging a patch for an older line after a newer minor is normal.
        result = add(manifest_with("1.1.0", "1.0.0"), "1.0.1")
        self.assertEqual(
            [r["version"] for r in result["releases"]],
            ["1.1.0", "1.0.1", "1.0.0"],
        )

    def test_republishing_a_version_replaces_it_rather_than_duplicating(self):
        # Re-running a failed workflow must not leave two entries for one
        # version — the dashboard would render the release twice.
        first = add(manifest_with("1.0.0"), "1.1.0", notes="First attempt.")
        second = add(first, "1.1.0", notes="Corrected.")

        versions = [r["version"] for r in second["releases"]]
        self.assertEqual(versions, ["1.1.0", "1.0.0"])
        self.assertEqual(second["releases"][0]["notes"], "Corrected.")

    def test_republishing_with_the_v_prefix_still_matches(self):
        first = add(manifest_with(), "1.1.0")
        second = add(first, "v1.1.0")
        self.assertEqual(len(second["releases"]), 1)

    def test_the_comment_block_survives(self):
        # It is the only documentation of what the file is for.
        result = add(manifest_with("1.0.0"), "1.1.0")
        self.assertEqual(result["$comment"], ["kept"])

    def test_an_empty_manifest_is_a_valid_starting_point(self):
        result = add({}, "0.1.0")
        self.assertEqual(len(result["releases"]), 1)

    def test_every_field_the_dashboard_reads_is_written(self):
        entry = add({}, "1.0.0", url="https://example.test/v1")["releases"][0]
        for field in ["version", "published_at", "url", "breaking", "notes"]:
            self.assertIn(field, entry, f"{field} is missing; the dashboard reads it")


class ArtifactDigests(unittest.TestCase):
    def test_digests_are_read_from_sha256sum_files(self):
        # The workflow downloads `<name>.tar.gz.sha256` files next to the
        # tarballs; each is one line of `sha256sum` output. Nested directories
        # happen when download-artifact unpacks per-artifact subdirectories.
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "nested").mkdir()
            (root / "a.tar.gz.sha256").write_text(f"{DIGEST}  a.tar.gz\n")
            (root / "nested" / "b.tar.gz.sha256").write_text(f"{'b' * 64}  b.tar.gz\n")

            digests = add_release.collect_digests(root)

        self.assertEqual(
            digests,
            {"a.tar.gz": {"sha256": DIGEST}, "b.tar.gz": {"sha256": "b" * 64}},
        )

    def test_a_malformed_digest_fails_the_publish(self):
        # A release published with a garbled digest would make that artifact
        # unverifiable forever, so the workflow must fail while a human can
        # still fix it.
        for content in [
            "not-hex  a.tar.gz\n",
            f"{'a' * 63}  a.tar.gz\n",  # too short
            f"{DIGEST}\n",  # no filename
            f"{DIGEST}  a.zip\n",  # not a tarball
        ]:
            with self.subTest(content=content), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                (root / "a.tar.gz.sha256").write_text(content)
                with self.assertRaises(ValueError):
                    add_release.collect_digests(root)

    def test_an_empty_digests_dir_is_an_error_not_a_silent_omission(self):
        # Passing --digests-dir and matching nothing means the workflow layout
        # changed; publishing without digests would look like success.
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                add_release.collect_digests(pathlib.Path(tmp))

    def test_duplicate_filenames_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "one").mkdir()
            (root / "two").mkdir()
            (root / "one" / "a.tar.gz.sha256").write_text(f"{DIGEST}  a.tar.gz\n")
            (root / "two" / "a.tar.gz.sha256").write_text(f"{'b' * 64}  a.tar.gz\n")
            with self.assertRaises(ValueError):
                add_release.collect_digests(root)

    def test_digests_land_in_the_entry_and_absence_leaves_no_key(self):
        with_digests = add(
            {}, "1.0.0", artifacts={"a.tar.gz": {"sha256": DIGEST}}
        )["releases"][0]
        self.assertEqual(with_digests["artifacts"], {"a.tar.gz": {"sha256": DIGEST}})

        # Older entries and digest-less publishes keep the old shape, which is
        # what the server's serde default tolerates.
        without = add({}, "1.0.0")["releases"][0]
        self.assertNotIn("artifacts", without)

    def test_republishing_replaces_digests_along_with_the_entry(self):
        first = add({}, "1.0.0", artifacts={"a.tar.gz": {"sha256": DIGEST}})
        second = add(first, "1.0.0", artifacts={"a.tar.gz": {"sha256": "b" * 64}})
        self.assertEqual(
            second["releases"][0]["artifacts"]["a.tar.gz"]["sha256"], "b" * 64
        )


class BreakingReleases(unittest.TestCase):
    def test_notes_that_announce_manual_steps_set_the_flag(self):
        for notes in [
            "This is a **breaking** change.",
            "Requires a manual step before upgrading.",
            "ACTION REQUIRED: rotate your keys.",
        ]:
            with self.subTest(notes=notes):
                self.assertTrue(add_release.looks_breaking(notes))

    def test_ordinary_notes_do_not(self):
        self.assertFalse(add_release.looks_breaking("- Fixed a crash on startup."))

    def test_the_flag_can_be_forced_on_regardless(self):
        entry = add({}, "1.0.0", notes="Nothing alarming here.", breaking=True)["releases"][0]
        self.assertTrue(entry["breaking"])


if __name__ == "__main__":
    unittest.main()
