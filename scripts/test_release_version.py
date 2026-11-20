import tempfile
from pathlib import Path
import unittest

from release_version import check_bump, project_version, version_tuple


def release(tag, *, draft=False, prerelease=False):
    return dict(tag_name=tag, draft=draft, prerelease=prerelease)


class ReleaseVersion(unittest.TestCase):
    def test_first_exact_bumps_from_zero(self):
        for bump, expected, wrong in [
            ("major", "1.0.0", "0.1.0"),
            ("minor", "0.1.0", "0.0.1"),
            ("patch", "0.0.1", "0.1.0"),
        ]:
            with self.subTest(bump=bump):
                check_bump(expected, bump, [[]])
                with self.assertRaisesRegex(ValueError, rf"from 0\.0\.0 must be {expected}"):
                    check_bump(wrong, bump, [[]])

    def test_first_release_ignores_nonstable_versions(self):
        pages = [[
            release("v1.0.0", prerelease=True),
            release("v0.1.1", draft=True),
            release("v2.0.0-beta.1"),
        ]]
        check_bump("0.1.0", "minor", pages)

    def test_exact_bump_from_highest_stable_release_across_pages(self):
        pages = [[release("v1.9.9")], [release("v1.10.2"), release("v2.0.0", draft=True)]]
        check_bump("2.0.0", "major", [[release("v1.10.2")]])
        check_bump("1.11.0", "minor", pages)
        check_bump("1.10.3", "patch", pages)
        with self.assertRaisesRegex(ValueError, "must be 1.10.3"):
            check_bump("1.11.0", "patch", pages)
        with self.assertRaisesRegex(ValueError, "must be 1.11.0"):
            check_bump("1.11.1", "minor", pages)
        with self.assertRaisesRegex(ValueError, "already exists"):
            check_bump("2.0.0", "major", pages)

    def test_invalid_versions(self):
        for value in ["1.01.0", "1.2.3-rc.1", "1.2", "1.2.3.4", "4294967296.0.0"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                version_tuple(value)

    def test_project_and_lockfile_must_agree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[package]\nname = "spofify-desktop"\nversion = "0.2.0"\n')
            lock = root / "Cargo.lock"
            lock.write_text('[[package]]\nname = "spofify-desktop"\nversion = "0.1.0"\n')
            with self.assertRaisesRegex(ValueError, "must agree"):
                project_version(root)
            lock.write_text(lock.read_text().replace("0.1.0", "0.2.0"))
            self.assertEqual(project_version(root), "0.2.0")


if __name__ == "__main__":
    unittest.main()
