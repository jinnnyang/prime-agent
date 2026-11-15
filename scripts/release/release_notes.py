"""Render a GitHub release body.

Two shapes, both ending in an install/update footer:

- stable (`--version X.Y.Z`): the folded CHANGELOG section for the released
  version. The stable attach step publishes the release with `body_path`
  pointing at this script's output, so a release is never published with an
  empty body again (the changelog fold is the release PR's review surface;
  its section IS the release notes' content).
- nightly (`--nightly`): the `.changes/` bullets the build carries (what the
  next stable release will ship) since the newest released version, plus the
  beta channel's install route. The rolling `nightly` release is the beta
  channel's only entry, so this body is what a reader lands on; the beta
  assets are named `install-beta.*` and the channel switch is `--nightly`.

Usage:
    python3 scripts/release/release_notes.py --version X.Y.Z \
        [--changelog CHANGELOG.md]
    python3 scripts/release/release_notes.py --nightly \
        [--changelog CHANGELOG.md] [--changes .changes]

Fails closed: a missing or empty section for the version exits 1, and so does
a changelog with no released version heading in nightly mode.
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


NIGHTLY_INSTALL_FOOTER = """## Install / update (nightly channel)

Fresh install (Windows):

    irm https://github.com/jinnnyang/prime-agent/releases/download/nightly/install-beta.ps1 | iex

Fresh install (macOS / Linux):

    curl -fsSL https://github.com/jinnnyang/prime-agent/releases/download/nightly/install-beta.sh | sh

Existing installs: `prime-agent update --nightly` (or `prime-agent update --check --nightly` first).

Back to stable: `prime-agent update --stable`."""

RELEASED_VERSION_RE = re.compile(r"^## \[([0-9]+\.[0-9]+\.[0-9]+)\]", re.M)


def newest_released_version(changelog: str) -> str:
    """The newest `## [X.Y.Z]` heading: CHANGELOG only carries released
    versions (prereleases never get a section), so the first one is the base
    the next stable release starts from."""
    match = RELEASED_VERSION_RE.search(changelog)
    if match is None:
        raise SystemExit("no released version section in the CHANGELOG")
    return match.group(1)


def pending_bullets(changes_dir: Path) -> list[str]:
    """The `.changes/` bullets the build carries, in path order (the fold's
    own order is the git add-date; for a nightly body a stable order is
    enough). `README.md` documents the pattern and is never a fragment."""
    if not changes_dir.is_dir():
        return []
    bullets: list[str] = []
    for fragment in sorted(changes_dir.glob("*.md")):
        if fragment.name == "README.md":
            continue
        bullets.extend(
            line.strip()
            for line in fragment.read_text(encoding="utf-8").splitlines()
            if line.strip().startswith("- ")
        )
    return bullets


def render(changelog: str, version: str) -> str:
    return f"{extract_section(changelog, version)}\n\n{INSTALL_FOOTER}\n"


def render_nightly(changelog: str, changes_dir: Path) -> str:
    base = newest_released_version(changelog)
    bullets = pending_bullets(changes_dir)
    body = "\n".join(bullets) if bullets else "No user-facing changes yet."
    return f"## What changed since {base}\n\n{body}\n\n{NIGHTLY_INSTALL_FOOTER}\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="the released X.Y.Z (stable body)")
    parser.add_argument(
        "--nightly",
        action="store_true",
        help="render the nightly body instead of a stable version's section",
    )
    parser.add_argument("--changelog", default="CHANGELOG.md")
    parser.add_argument("--changes", default=".changes", help="the fragment directory")
    args = parser.parse_args()
    if not args.nightly and args.version is None:
        parser.error("--version is required unless --nightly is given")
    changelog = Path(args.changelog).read_text(encoding="utf-8")
    if args.nightly:
        sys.stdout.write(render_nightly(changelog, Path(args.changes)))
    else:
        sys.stdout.write(render(changelog, args.version))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
