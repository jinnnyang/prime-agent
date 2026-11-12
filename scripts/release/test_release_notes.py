"""The release-notes extractor's battery: the section carve-out is the
content contract, and the fail-closed rule is first - an empty body must
never regress, so a missing or empty section exits nonzero."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("release_notes.py")

CHANGELOG = """# Changelog

Release notes for Prime Agent.

## [0.11.0] - 2026-11-12

- Fixed the clipboard mojibake.
- A kernel venv rebuild no longer deletes a venv a running kernel executes from.

## [0.10.4] - 2026-11-10

- Older bullet.
"""


def run(args: list[str], changelog_dir: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        cwd=changelog_dir, capture_output=True, text=True, check=False,
    )


class ExtractorTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        Path(self.dir.name, "CHANGELOG.md").write_text(CHANGELOG, encoding="utf-8")

    def test_the_version_section_becomes_the_body_bullets_kept_verbatim(self) -> None:
        out = run(["--version", "0.11.0"], Path(self.dir.name))
        self.assertEqual(out.returncode, 0, out.stderr)
        body = out.stdout
        self.assertIn("- Fixed the clipboard mojibake.", body)
        self.assertIn("no longer deletes a venv a running kernel executes from", body)
        self.assertNotIn("## [0.11.0]", body, "the section header is not body content")
        self.assertNotIn("Older bullet.", body, "another version's section never leaks")

    def test_the_install_footer_rides_every_body(self) -> None:
        out = run(["--version", "0.11.0"], Path(self.dir.name))
        self.assertIn("## Install / update", out.stdout)
        self.assertIn("install.ps1", out.stdout)
        self.assertIn("prime-agent update", out.stdout)

    def test_a_missing_version_section_fails_closed(self) -> None:
        out = run(["--version", "9.9.9"], Path(self.dir.name))
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("no CHANGELOG section for 9.9.9", out.stderr)

    def test_an_empty_section_fails_closed(self) -> None:
        changelog = CHANGELOG.replace(
            "## [0.10.4] - 2026-11-10\n\n- Older bullet.\n",
            "## [0.10.4] - 2026-11-10\n")
        Path(self.dir.name, "CHANGELOG.md").write_text(changelog, encoding="utf-8")
        out = run(["--version", "0.10.4"], Path(self.dir.name))
        self.assertNotEqual(out.returncode, 0)

    def test_the_last_section_extracts_without_a_trailing_heading(self) -> None:
        changelog = CHANGELOG.replace("## [0.10.4] - 2026-11-10\n\n- Older bullet.\n\n", "")
        Path(self.dir.name, "CHANGELOG.md").write_text(changelog, encoding="utf-8")
        out = run(["--version", "0.11.0"], Path(self.dir.name))
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("Fixed the clipboard mojibake", out.stdout)


if __name__ == "__main__":
    unittest.main()
