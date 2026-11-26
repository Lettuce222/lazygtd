import pathlib
import tempfile
import unittest

from release_plan import next_tag, read_refs, stamp


class ReleasePlanTest(unittest.TestCase):
    def test_allocates_a_new_preview_when_cargo_version_is_unchanged(self):
        refs = {"v0.1.0-preview.8": "first", "v0.1.0-preview.9": "second"}
        self.assertEqual(
            next_tag("0.1.0-preview.8", refs, refs, "third", lambda *_: False),
            "v0.1.0-preview.10",
        )

    def test_retries_finish_the_same_tag_only_once(self):
        refs = {"v0.1.0-preview.8": "first", "v0.1.0-preview.9": "second"}
        self.assertEqual(
            next_tag("0.1.0-preview.8", refs, ["v0.1.0-preview.8"], "second", lambda *_: False),
            "v0.1.0-preview.9",
        )
        self.assertEqual(next_tag("0.1.0-preview.8", refs, refs, "second", lambda *_: False), "")

    def test_an_older_rerun_cannot_supersede_a_newer_release(self):
        refs = {"v0.1.0-preview.9": "newer"}
        self.assertEqual(
            next_tag("0.1.0-preview.8", refs, refs, "older", lambda a, b: (a, b) == ("older", "newer")),
            "",
        )

    def test_version_bumps_and_stable_patch_releases(self):
        refs = {"v0.1.0": "first"}
        self.assertEqual(
            next_tag("0.1.0", refs, refs, "second", lambda *_: False), "v0.1.1"
        )
        self.assertEqual(
            next_tag("0.2.0-preview.1", refs, refs, "second", lambda *_: False), "v0.2.0-preview.1"
        )

    def test_annotated_tags_use_the_peeled_commit(self):
        self.assertEqual(read_refs(["v0.1.0\tobject\tcommit", "v0.2.0\tother\t"]), {
            "v0.1.0": "commit",
            "v0.2.0": "other",
        })

    def test_stamps_only_the_root_package_in_the_manifest_and_lockfile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            package = 'name = "lazygtd"\nversion = "0.1.0-preview.8"\n'
            dependency = 'name = "other"\nversion = "0.1.0-preview.8"\n'
            manifest = root / "Cargo.toml"
            lock = root / "Cargo.lock"
            manifest.write_text("[package]\n" + package, encoding="utf-8")
            lock.write_text("[[package]]\n" + package + "\n[[package]]\n" + dependency, encoding="utf-8")
            stamp("v0.1.0-preview.9", root)
            self.assertIn(package.replace("preview.8", "preview.9"), manifest.read_text())
            self.assertIn(package.replace("preview.8", "preview.9"), lock.read_text())
            self.assertIn(dependency, lock.read_text())
