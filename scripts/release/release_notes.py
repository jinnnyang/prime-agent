"""Render the stable GitHub release body: the folded CHANGELOG section for
the released version plus the fixed install/update footer.

The stable attach step publishes the release with `body_path` pointing at
this script's output, so a release is never published with an empty body
again (the changelog fold is the release PR's review surface; its section
IS the release notes' content) and the update path is always one glance
away.

Usage:
    python3 scripts/release/release_notes.py --version X.Y.Z \
        [--changelog CHANGELOG.md]

Fails closed: a missing or empty section for the version exits 1.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

SECTION_START_RE = re.compile(r"^## \[", re.M)


def extract_section(changelog: str, version: str) -> str:
    """The `## [version]` section's body: the header line dropped, the
    bullets (and any prose the fold kept) kept verbatim, up to the next
    section heading."""
    start = re.search(rf"^## \[{re.escape(version)}\].*$", changelog, re.M)
    if start is None:
        raise SystemExit(
            f"no CHANGELOG section for {version}: the release notes would be empty")
    next_start = SECTION_START_RE.search(changelog, start.end())
    body = changelog[start.end():next_start.start() if next_start else len(changelog)]
    body = body.strip("\n")
    if not body.strip():
        raise SystemExit(f"the CHANGELOG section for {version} is empty")
    return body


INSTALL_FOOTER = """## Install / update

Fresh install (Windows):

    irm https://github.com/jinnnyang/prime-agent/releases/latest/download/install.ps1 | iex

Fresh install (macOS / Linux):

    curl -fsSL https://github.com/jinnnyang/prime-agent/releases/latest/download/install.sh | sh

Existing installs: `prime-agent update` (or `prime-agent update --check` first)."""


def render(changelog: str, version: str) -> str:
    return f"{extract_section(changelog, version)}\n\n{INSTALL_FOOTER}\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="the released X.Y.Z")
    parser.add_argument("--changelog", default="CHANGELOG.md")
    args = parser.parse_args()
    changelog = Path(args.changelog).read_text(encoding="utf-8")
    sys.stdout.write(render(changelog, args.version))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
