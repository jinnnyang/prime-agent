"""Persistent harness-state helpers for Prime Agent's RLM kernel.

The state model is intentionally small: it records prompt notes, memory,
skills, subagent specs, and refinement events in the session-local harness
store by default; pass ``global_=True`` for the cross-session global store.
Execution still belongs to Prime Agent's TypeScript host and the existing
``rlm.spawn`` recursion bridge.
"""

from __future__ import annotations

import contextlib
import copy
import json
import math
import os
import re
import stat
import sys
import time
import unicodedata
from collections.abc import Iterator
from dataclasses import asdict, dataclass, field, fields
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4
from typing import Any, Literal

from .factory import require_factory_enabled, validate_factory_spec

if sys.platform == "darwin":
    import fcntl

HarnessKind = Literal["prompt", "memory", "skill", "subagent", "factory"]
HarnessScope = Literal["local", "global"]

_DEFAULT_FILE_NAME = "harness_state.json"
_DEFAULT_HARNESS_DIR_NAME = "harness"
_KINDS: tuple[HarnessKind, ...] = ("prompt", "memory", "skill", "subagent", "factory")
# Kinds whose truth lives in files, not in the harness-state entry store.
_PAGE_BACKED_KINDS: frozenset[HarnessKind] = frozenset({"memory"})
_state_cache: dict[tuple[Path, HarnessScope], "HarnessState"] = {}
_LOCK_STALE_AFTER = 10.0
_LOCK_ATTEMPTS = 50
_LOCK_RETRY_INTERVAL = 0.02
_PROCESS_TOKEN = uuid4().hex


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


_SLUG_CHARS = re.compile(r"^[A-Za-z0-9._-]{1,120}$")


def _valid_slug(slug: str) -> bool:
    """Whether a slug is safe as a file name; mirrors `memory::is_valid_slug`."""
    return bool(
        _SLUG_CHARS.match(slug)
        and ".." not in slug
        and not slug.startswith(".")
        and not slug.endswith(".md")
    )


def _require_slug(slug: str) -> None:
    if not _valid_slug(slug):
        raise ValueError(f"memory slug {slug!r} is not a safe file name")


def _slug(raw: str, fallback: str) -> str:
    normalized = "".join(ch.lower() if ch.isalnum() else "_" for ch in raw.strip())
    normalized = "_".join(part for part in normalized.split("_") if part)
    return (normalized or fallback)[:80]


_CJK_TERM_CHARS = re.compile(
    r"[\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uac00-\ud7af"
    r"\U00020000-\U0002a6df\U0002a700-\U0002b73f\U0002b740-\U0002b81f"
    r"\U0002b820-\U0002ceaf\U0002ceb0-\U0002ebef\U0002ebf0-\U0002ee5f"
    r"\U0002f800-\U0002fa1f\U00030000-\U0003134f\U00031350-\U000323af"
    r"\U000323b0-\U0003347f]"
)


def _harness_query_runs(text: str) -> list[str]:
    """Split lowercase text into word runs.

    Letters, digits, and combining marks of any script share a run;
    punctuation and symbols end it. Runs break only at CJK boundaries:
    accented Latin stays whole (naïve) while spacing-free CJK is cut
    apart from adjacent words it would otherwise swallow (修复login).
    """
    runs: list[str] = []
    run: list[str] = []
    run_is_cjk = False
    for ch in text:
        if unicodedata.category(ch).startswith("M") or ch.isalnum():
            ch_is_cjk = bool(_CJK_TERM_CHARS.match(ch))
            if run and ch_is_cjk != run_is_cjk:
                runs.append("".join(run))
                run = []
            run_is_cjk = ch_is_cjk
            run.append(ch)
        elif run:
            runs.append("".join(run))
            run = []
    if run:
        runs.append("".join(run))
    return runs


def _harness_query_terms(query: str) -> list[str]:
    """Tokenize a search query into lowercase substring terms.

    Letters and digits of every script form terms; punctuation and symbols
    only separate them, so ``worktree?`` never ranks entries by question
    marks. CJK runs carry no spaces between words, so each run becomes
    overlapping bigrams: ``修复登录`` yields ``修复``/``复登``/``登录`` and
    still matches an entry containing ``登录故障``. Each term counts once.
    Minimum lengths stay below the digest builder's four-character cut
    because ``search`` tokenizes explicit queries, not mined conversation:
    three ASCII characters keep real terms (rlm, api, cli), two characters
    keep short words of other scripts (мир), and single characters are
    terms only for CJK, where one character is a word.
    """
    terms: list[str] = []
    seen: set[str] = set()
    for run in _harness_query_runs(query.lower()):
        if _CJK_TERM_CHARS.search(run):
            # Bigrams keep whitespace-free CJK findable without single
            # characters matching too loosely.
            candidates = [run[i : i + 2] for i in range(len(run) - 1)] or [run]
        elif run.isascii():
            candidates = [run] if len(run) >= 3 else []
        else:
            # Other scripts space out words: lone characters match too
            # broadly, so two characters is the floor.
            candidates = [run] if len(run) >= 2 else []
        for term in candidates:
            if term not in seen:
                seen.add(term)
                terms.append(term)
    return terms


def _agent_dir() -> Path:
    raw = (
        os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        or os.environ.get("PI_CODING_AGENT_DIR")
        or str(Path.home() / ".prime" / "agent")
    )
    return Path(raw).expanduser().resolve()


_MEMORY_TYPES: tuple[str, ...] = ("world", "model", "observation", "experience")
_MEMORY_DEFAULT_TYPE = "experience"
_MEMORY_DIR_NAME = "memory"
_MEMORY_PAGES_DIR_NAME = "pages"
_MEMORY_ARCHIVE_DIR_NAME = "archive"
_MEMORY_ARCHIVE_LOG = "_archive.md"
# Pages that are structure nodes, not knowledge units.
_MEMORY_SKIPPED_DIRS = frozenset({"_entities"})


def _memory_dir(root: Path | None = None) -> Path:
    """The memory directory: the caller's when given, else the agent dir's."""
    if root is not None:
        return root
    return _agent_dir() / _MEMORY_DIR_NAME


def _memory_root_for(state_file: Path | None) -> Path:
    """Where a state's pages live, derived from the state it belongs to.

    `harness_state.json` sits under `<agent dir>/harness/`, so pages for it are
    `<agent dir>/memory/`. Following the state instead of the process environment
    keeps two states in one process from sharing (and corrupting) one page store.
    """
    if state_file is None:
        return _memory_dir()
    parent = state_file.parent
    base = parent.parent if parent.name == "harness" else parent
    return base / _MEMORY_DIR_NAME


def _memory_pages_dir(root: Path | None = None) -> Path:
    return _memory_dir(root) / _MEMORY_PAGES_DIR_NAME


def _split_page(text: str) -> tuple[dict[str, str], str]:
    """The page's top-level frontmatter fields and its body."""
    if not text.startswith("---"):
        return {}, text
    end = text.find("\n---", 3)
    if end == -1:
        return {}, text
    block = text[3:end]
    body = text[end + 4 :].lstrip("-\r\n")
    fields: dict[str, str] = {}
    for line in block.splitlines():
        if not line or line[0] in " \t-#":
            continue
        key, sep, value = line.partition(":")
        if not sep:
            continue
        fields[key.strip()] = value.strip().strip("'\"")
    return fields, body


def _page_files(root: Path | None = None) -> dict[str, Path]:
    """slug -> path for every knowledge page on disk (structure nodes skipped)."""
    root = _memory_pages_dir(root)
    found: dict[str, Path] = {}
    if not root.is_dir():
        return found
    for path in sorted(root.rglob("*.md")):
        if _MEMORY_SKIPPED_DIRS.intersection(path.parts):
            continue
        try:
            fields, _ = _split_page(path.read_text(encoding="utf-8"))
        except OSError:
            continue
        found[fields.get("slug") or path.stem] = path
    return found


def _yaml_scalar(value: str) -> str:
    """A frontmatter scalar that survives a YAML parse with any punctuation."""
    text = str(value).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{text}"'


def _merge_frontmatter(block: list[str], updates: dict[str, str]) -> list[str]:
    """Rewrite the given top-level keys, keeping every other line verbatim.

    Nested blocks (``evidence:``, ``relations:``, ``history:``) belong to a parent
    key and are kept as-is unless that parent key is being rewritten, so an edit
    never silently drops a page's provenance.
    """
    out: list[str] = []
    written: set[str] = set()
    skip_children = False
    for line in block:
        nested = bool(line) and (line[0] in " \t" or line.startswith("-"))
        if nested and skip_children:
            continue
        if nested:
            out.append(line)
            continue
        key, sep, _ = line.partition(":")
        key = key.strip()
        if not sep:
            out.append(line)
            continue
        if key in updates:
            skip_children = True
            weight = updates[key]
            if weight:
                out.append(f"{key}: {weight}")
                written.add(key)
            continue
        skip_children = False
        out.append(line)
    for key, value in updates.items():
        if key not in written and value:
            out.append(f"{key}: {value}")
    return out


def _write_memory_page(
    slug: str,
    *,
    page_type: str,
    title: str,
    content: str,
    hook: str | None = None,
    status: str = "draft",
    merge_path: Path | None = None,
    root: Path | None = None,
) -> Path:
    """Write a page; an existing page keeps every field the caller omits."""
    if page_type not in _MEMORY_TYPES:
        raise ValueError(
            f"unknown memory type {page_type!r}; expected one of {list(_MEMORY_TYPES)}"
        )
    block: list[str] = []
    if merge_path is not None and merge_path.exists():
        try:
            text = merge_path.read_text(encoding="utf-8")
        except OSError:
            text = ""
        if text.startswith("---"):
            close = text.find("\n---", 3)
            if close != -1:
                block = text[3:close].strip("\r\n").splitlines()
    updates = {
        "slug": _yaml_scalar(slug),
        "type": page_type,
        "title": _yaml_scalar(title),
        "status": status,
        "updated": datetime.now(timezone.utc).date().isoformat(),
    }
    if hook is not None:
        updates["hook"] = _yaml_scalar(hook)
    lines = ["---", *_merge_frontmatter(block, updates), "---"]
    _require_slug(slug)
    target = _memory_pages_dir(root) / page_type / f"{slug}.md"
    _write_text_atomically(target, "\n".join(lines) + "\n\n" + content.strip() + "\n")
    return target


def _removal_refusal(slug: str, root: Path | None = None) -> str | None:
    """Why the rules would keep this page, or None when it may be archived.

    The same preconditions `memory::dream::removal_eligible` applies: a page with
    proof, an edge, a real body, a dispute, or a recent write is not removable,
    and the kernel must not be the way around that.
    """
    files = _page_files(root)
    path = files.get(slug)
    if path is None:
        return "no such page"
    if any(part in _MEMORY_SKIPPED_DIRS for part in path.parts):
        return "structure node"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return f"unreadable page: {path}"
    fields, body = _split_page(text)
    if _block_entries(_frontmatter_lines(text), "evidence"):
        return "it carries evidence"
    if _block_entries(_frontmatter_lines(text), "relations"):
        return "it declares relations"
    for other_slug, other in files.items():
        if other_slug == slug:
            continue
        try:
            other_text = other.read_text(encoding="utf-8")
        except OSError:
            continue
        if slug in {
            line.split("target:", 1)[1].strip().strip("'\"")
            for line in _frontmatter_lines(other_text)
            if line.lstrip().startswith("target:")
        }:
            return f"{other_slug} points at it"
    if len(body.strip()) >= 120:
        return "its body is long enough to keep"
    updated = fields.get("updated") or ""
    if len(updated) >= 10:
        try:
            written = datetime.fromisoformat(updated[:10]).date()
        except ValueError:
            return "its last write date is unreadable"
        if (datetime.now(timezone.utc).date() - written).days < 14:
            return "it was written less than two weeks ago"
    else:
        return "it has no last-write date"
    return None


def _write_text_atomically(target: Path, text: str) -> None:
    """Write beside the target and rename, so a crash never leaves half a page."""
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    try:
        temporary.write_text(text, encoding="utf-8")
        os.replace(temporary, target)
    finally:
        temporary.unlink(missing_ok=True)


def _tombstone_memory_page(
    slug: str, reason: str = "archived", root: Path | None = None
) -> bool:
    """Archive a page instead of unlinking it: removal stays reversible."""
    path = _page_files(root).get(slug)
    if path is None:
        return False
    archive = _memory_dir(root) / _MEMORY_ARCHIVE_DIR_NAME
    archive.mkdir(parents=True, exist_ok=True)
    fields, _ = _split_page(path.read_text(encoding="utf-8"))
    path.replace(archive / path.name)
    title = fields.get("title", slug)
    log = _memory_dir(root) / _MEMORY_ARCHIVE_LOG
    if not log.exists():
        log.write_text("# Archive\n\n", encoding="utf-8")
    with log.open("a", encoding="utf-8") as handle:
        stamp = datetime.now(timezone.utc).date().isoformat()
        handle.write(f"- {slug} — {title} ({reason} {stamp})\n")
    return True


def _front_key_lines(text: str, key: str) -> list[str]:
    """The list entries under a frontmatter key, each with its continuation lines."""
    entries: list[list[str]] = []
    inside = False
    for line in _frontmatter_lines(text):
        if line.startswith(f"{key}:"):
            inside = True
            continue
        if not inside:
            continue
        if line.startswith("  - ") or line.startswith("- "):
            entries.append([line])
        elif line.startswith("    ") and entries:
            entries[-1].append(line)
        else:
            inside = False
    return ["\n".join(entry) for entry in entries]


def _reindent(entry: list[str], indent: str) -> list[str]:
    """The entry respelled in the target list's indentation, structure preserved."""
    base = len(entry[0]) - len(entry[0].lstrip())
    return [indent + line[base:] for line in entry]


def _block_bounds(block: list[str], key: str) -> tuple[int, int] | None:
    """Start and end (exclusive) of a top-level key's lines, if the page has it."""
    start = None
    for index, line in enumerate(block):
        if line.startswith(f"{key}:"):
            start = index
            break
    if start is None:
        return None
    end = len(block)
    for index in range(start + 1, len(block)):
        line = block[index]
        # A list item at column 0 still belongs to the key above it.
        if line.strip() and not line.startswith((" ", "\t", "- ")):
            end = index
            break
    return start, end


def _block_entries(block: list[str], key: str) -> list[list[str]]:
    """The list entries under a key, each with its continuation lines.

    Continuations are recognised by indentation, not by a fixed width: a page may
    write ``- kind: x`` at column 0 with two-space fields, and treating only four
    spaces as a continuation drops the fields and then the entries after them.
    """
    bounds = _block_bounds(block, key)
    if bounds is None:
        return []
    start, end = bounds
    entries: list[list[str]] = []
    for line in block[start + 1 : end]:
        stripped = line.lstrip()
        if stripped.startswith("- "):
            entries.append([line])
        elif entries and line[:1] in (" ", "\t"):
            entries[-1].append(line)
        elif stripped:
            break
    return entries


def _entry_identity(entry: list[str]) -> str:
    """What makes two evidence entries the same: its lines, whitespace-insensitive."""
    return " ".join(" ".join(piece.split()) for piece in entry)


_PROMOTION_STEPS: dict[str, str] = {
    "experience": "observation",
    "observation": "model",
    "model": "world",
}

# Evidence kinds a machine can re-check; a page that never leaves the session log
# cannot become world, whatever it says.
_MACHINE_EVIDENCE_KINDS = frozenset({"web", "file", "cmd"})

def _promotion_verdict(slug: str, root: Path | None = None) -> tuple[str | None, str]:
    """The type this page has earned, or None and why it has not.

    A type is a decay bracket, not a provenance: evidence accumulates and the
    page moves up the ladder, so a preference stated repeatedly becomes a model
    before anyone proves it. Nothing demotes; an unread page simply decays out.
    """
    path = _page_files(root).get(slug)
    if path is None:
        return None, "no such page"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None, f"unreadable page: {path}"
    fields, body = _split_page(text)
    kind = fields.get("type") or "experience"
    target = _PROMOTION_STEPS.get(kind)
    if target is None:
        return None, f"{kind} is the top of the ladder"
    if fields.get("status") == "disputed":
        return None, "the page is disputed"
    evidence = _block_entries(_frontmatter_lines(text), "evidence")
    kinds: set[str] = set()
    for entry in evidence:
        for line in entry:
            for found in re.finditer(r"kind:\s*([^,}\n]+)", line):
                kinds.add(found.group(1).strip().strip("'\""))
    if kind == "experience":
        if len(evidence) < 2:
            return None, "needs two independent pieces of evidence"
    elif kind == "observation":
        if len(kinds) < 2:
            return None, f"needs two kinds of evidence, has {sorted(kinds) or 'none'}"
    else:
        if not (kinds & _MACHINE_EVIDENCE_KINDS):
            return None, "needs web, file, or cmd evidence a machine can re-check"
    return target, f"{len(evidence)} evidence entries ({sorted(kinds) or 'none'})"

def _promote_memory_page(slug: str, target: str, root: Path | None = None) -> bool:
    """Move a page to a slower-decaying type, recording why it moved.

    Promotion must stay on the ladder (one step up, never sideways or down) and
    must leave exactly one page per slug: a stale copy in the old directory would
    give the loader two candidates and pick by directory order.
    """
    path = _page_files(root).get(slug)
    if path is None:
        return False
    if _PROMOTION_STEPS.get(path.parent.name) != target:
        return False
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return False
    block = _frontmatter_lines(text)
    _fields, body = _split_page(text)
    updates = {
        "type": target,
        "updated": datetime.now(timezone.utc).date().isoformat(),
    }
    block = _merge_frontmatter(block, updates)
    history = f'  - {{at: {updates["updated"]}, from: {path.parent.name}, to: {target}, reason: "promoted"}}'
    bounds = _block_bounds(block, "history")
    if bounds is None:
        block.extend(["history:", history])
    else:
        block.insert(bounds[1], history)
    destination = _memory_pages_dir(root) / target / f"{slug}.md"
    _write_text_atomically(
        destination, "---\n" + "\n".join(block) + "\n---\n\n" + body.strip() + "\n"
    )
    if destination != path:
        with contextlib.suppress(OSError):
            path.unlink()
    return True

def merge_pages(keep: str, absorb: str, root: Path | None = None) -> bool:
    """Merge one page into another the pass decided says the same thing.

    Body lines survive once, evidence entries carry over with their continuation
    lines, references that pointed at the absorbed slug follow the survivor, and
    the absorbed page is archived with a tombstone. A merge must never lose where
    a claim came from, which is why it is this function and not a hand edit.
    """
    if keep == absorb:
        return False
    files = _page_files(root)
    kept, absorbed_path = files.get(keep), files.get(absorb)
    if kept is None or absorbed_path is None:
        return False
    try:
        kept_text = kept.read_text(encoding="utf-8")
        absorbed_text = absorbed_path.read_text(encoding="utf-8")
    except OSError:
        return False

    lines = [line for line in _split_page(kept_text)[1].splitlines() if line.strip()]
    for line in _split_page(absorbed_text)[1].splitlines():
        if line.strip() and line not in lines:
            lines.append(line)

    block = _frontmatter_lines(kept_text)
    present = {_entry_identity(entry) for entry in _block_entries(block, "evidence")}
    carried = [
        entry
        for entry in _block_entries(_frontmatter_lines(absorbed_text), "evidence")
        if _entry_identity(entry) not in present
    ]
    if carried:
        bounds = _block_bounds(block, "evidence")
        if bounds is None:
            block.append("evidence:")
            bounds = (len(block) - 1, len(block))
        # Into the key's own block, before whatever follows it: appending to the
        # end of the frontmatter would file the evidence under the next key.
        insert_at = bounds[1]
        existing = _block_entries(block, "evidence")
        target_indent = (
            existing[0][0][: len(existing[0][0]) - len(existing[0][0].lstrip())]
            if existing
            else "  "
        )
        block[insert_at:insert_at] = [
            line for entry in carried for line in _reindent(entry, target_indent)
        ]

    _write_text_atomically(
        kept, "---\n" + "\n".join(block) + "\n---\n\n" + "\n".join(lines) + "\n"
    )
    _tombstone_memory_page(absorb, f"merged into {keep}", root=root)

    # A dangling reference is worse than a stale one: every page that named the
    # absorbed slug names the survivor instead. Exact match only, so a target
    # that merely ends with the slug is left alone.
    for slug, path in _page_files(root).items():
        if slug == absorb:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        front = _frontmatter_lines(text)
        rewritten = [
            line[: line.index("target:")] + 'target: ' + keep
            if line.lstrip().startswith("target:")
            and line.split("target:", 1)[1].strip().strip("'\"") == absorb
            else line
            for line in front
        ]
        if rewritten != front:
            _fields, body = _split_page(text)
            _write_text_atomically(
                path, "---\n" + "\n".join(rewritten) + "\n---\n\n" + body.strip() + "\n"
            )
    return True


def _memory_page_entry(slug: str, path: Path) -> HarnessEntry:
    """Project a page into the entry shape the read tools return."""
    fields, body = _split_page(path.read_text(encoding="utf-8"))
    return HarnessEntry(
        id=slug,
        kind="memory",
        title=fields.get("title", slug),
        content=body.strip(),
        path=fields.get("type", "other"),
        scope="global",
        reference={},
        arguments={},
        metadata={
            "hook": fields.get("hook", ""),
            "status": fields.get("status", ""),
            "updated": fields.get("updated") or fields.get("checked", ""),
            "page": str(path),
            "evidence": fields.get("evidence", ""),
            "weight": fields.get("weight", ""),
        },
        source="agent",
    )


def _frontmatter_lines(text: str) -> list[str]:
    """The page's frontmatter block as lines (empty when the page has none)."""
    if not text.startswith("---"):
        return []
    close = text.find("\n---", 3)
    if close == -1:
        return []
    return text[3:close].strip("\r\n").splitlines()


# Pages already counted as read by this process: a session that searches the same
# page five times must not inflate its weight five times.
_READ_PAGES: set[str] = set()


def _record_page_read(entry: HarnessEntry) -> None:
    """Count one read of a page: the host owns this field, the agent never writes it."""
    page = (entry.metadata or {}).get("page")
    if not page or entry.id in _READ_PAGES:
        return
    path = Path(page)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return
    _fields, body = _split_page(text)
    try:
        weight = float(_fields.get("weight") or 0.0)
    except ValueError:
        weight = 0.0
    updates = {
        "weight": f"{weight + 1.0:g}",
        "last_read": datetime.now(timezone.utc).date().isoformat(),
    }
    lines = ["---", *_merge_frontmatter(_frontmatter_lines(text), updates), "---"]
    try:
        _write_text_atomically(path, "\n".join(lines) + "\n\n" + body.strip() + "\n")
    except OSError:
        return
    _READ_PAGES.add(entry.id)


_LEGACY_IMPORT_MARKER = ".legacy-imported"
_LEGACY_BACKUP_NAME = "harness_state.pre-pages.json"


_DREAM_JOURNAL = "journal/dream.log.jsonl"
_DREAM_LOCK = ".dream.lock"
_DREAM_LOCK_STALE_SECS = 30 * 60
_DREAM_MIN_INTERVAL_SECS = 6 * 60 * 60
_DREAM_MAX_INTERVAL_SECS = 14 * 24 * 60 * 60


def _dream_journal_path(root: Path | None = None) -> Path:
    return _memory_dir(root) / _DREAM_JOURNAL


def dream_due(root: Path | None = None) -> tuple[bool, str]:
    """Whether a consolidation pass is due, read from the journal the pass writes.

    Mirrors `memory::dream::dream_due`: no pages means nothing to consolidate, and
    a recorded `next_at` in the future means the pass chose to wait.
    """
    if not _page_files(root):
        return False, "no pages"
    try:
        text = _dream_journal_path().read_text(encoding="utf-8")
    except OSError:
        return True, "no pass recorded yet"
    records = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except ValueError:
            continue
    if not records:
        return True, "no pass recorded yet"
    next_at = records[-1].get("next_at")
    now = int(datetime.now(timezone.utc).timestamp())
    if isinstance(next_at, (int, float)) and next_at > now:
        return False, f"next pass at {datetime.fromtimestamp(next_at, timezone.utc).isoformat()}"
    return True, "the last pass is ready for another"


def record_dream_pass(
    *,
    root: Path | None = None,
    visited: int = 0,
    merged: int = 0,
    promoted: int = 0,
    removed: int = 0,
    compressed: int = 0,
    why: str = "",
    next_hours: int = 12,
    seed: str = "pass",
) -> dict[str, Any]:
    """Record a finished consolidation pass and when it wants to run again.

    The pass cannot simply say "recorded": `dream_due` reads this file, so without
    it the store is due again on the very next render. The interval is clamped to
    the same floor and ceiling the design states, and the lock keeps two passes
    from writing competing `next_at` values.
    """
    now = int(datetime.now(timezone.utc).timestamp())
    hours = max(next_hours, _DREAM_MIN_INTERVAL_SECS // 3600)
    next_at = now + min(hours * 3600, _DREAM_MAX_INTERVAL_SECS)
    record = {
        "ts": now,
        "seed": seed,
        "visited": visited,
        "compressed": compressed,
        "merged": merged,
        "promoted": promoted,
        "removed": removed,
        "next_at": next_at,
        "why": why,
    }
    memory_dir = _memory_dir(root)
    lock = memory_dir / _DREAM_LOCK
    memory_dir.mkdir(parents=True, exist_ok=True)
    try:
        if lock.exists() and now - int(lock.read_text(encoding="utf-8").strip() or 0) < _DREAM_LOCK_STALE_SECS:
            raise RuntimeError("a consolidation pass holds the lock")
    except (OSError, ValueError):
        pass
    lock.write_text(f"{now}\n", encoding="utf-8")
    try:
        journal = _dream_journal_path()
        journal.parent.mkdir(parents=True, exist_ok=True)
        with journal.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")
    finally:
        lock.unlink(missing_ok=True)
    return record


def cli_dispatch(payload: dict[str, Any]) -> dict[str, Any]:
    """One memory operation, driven by `prime-agent memory` from the host CLI.

    The kernel owns the page rules; the CLI is a thin caller, so the two sides
    cannot disagree about when a page may be removed or what a merge carries.
    Returns `{"ok": bool, ...}`; a refused action carries its reason verbatim.
    """
    op = str(payload.get("op") or "")
    root = Path(payload["agent_dir"]) / _MEMORY_DIR_NAME if payload.get("agent_dir") else None
    try:
        if op == "status":
            pages = _page_files(root)
            due, reason = dream_due(root)
            kinds = sorted({path.parent.name for path in pages.values()})
            return {"ok": True, "pages": len(pages), "kinds": kinds, "due": due, "reason": reason}
        if op == "due":
            due, reason = dream_due(root)
            return {"ok": True, "due": due, "reason": reason}
        if op == "plan":
            promote, remove = [], []
            for slug in _page_files(root):
                target, why = _promotion_verdict(slug, root)
                if target is not None:
                    promote.append({"slug": slug, "to": target, "why": why})
                if _removal_refusal(slug, root) is None:
                    remove.append(slug)
            return {"ok": True, "promote": promote, "remove": sorted(remove)}
        if op == "promote":
            slug = str(payload.get("slug") or "")
            target = payload.get("to")
            verdict, why = _promotion_verdict(slug, root)
            if target is not None and target != verdict:
                # An explicit target is a request, not a licence: it still has to
                # be the step the evidence earned.
                return {
                    "ok": False,
                    "errors": [f"{slug}: {why} (asked for {target})"],
                    "promoted": False,
                }
            done = verdict is not None and _promote_memory_page(slug, verdict, root)
            return {"ok": done, "promoted": done, "slug": slug, "to": verdict if done else None,
                    "why": why}
        if op == "merge":
            keep, absorb = str(payload.get("keep") or ""), str(payload.get("absorb") or "")
            merged = merge_pages(keep, absorb, root)
            return {"ok": merged, "merged": merged, "keep": keep, "absorb": absorb}
        if op == "delete":
            slug = str(payload.get("slug") or "")
            refusal = _removal_refusal(slug, root)
            if refusal is not None:
                return {"ok": False, "errors": [f"memory page {slug!r} cannot be removed: {refusal}"]}
            return {"ok": _tombstone_memory_page(slug, "removed", root=root), "removed": slug}
        if op == "record-pass":
            record = record_dream_pass(
                root=root,
                visited=int(payload.get("visited") or 0),
                merged=int(payload.get("merged") or 0),
                promoted=int(payload.get("promoted") or 0),
                removed=int(payload.get("removed") or 0),
                compressed=int(payload.get("compressed") or 0),
                why=str(payload.get("why") or ""),
                next_hours=int(payload.get("next_hours") or 12),
                seed=str(payload.get("seed") or "pass"),
            )
            return {"ok": True, "record": record}
        return {"ok": False, "errors": [f"unknown memory op {op!r}"]}
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        return {"ok": False, "errors": [str(error)]}


def _refuse_local_memory(global_: bool) -> None:
    """Memory pages have one scope; say so instead of writing a global page."""
    if not global_:
        raise ValueError(
            "memory pages are global: pass global_=True (a session-local memory "
            "would be visible to every other session)"
        )


def _legacy_import_marker(root: Path | None = None) -> Path:
    return _memory_dir(root) / _LEGACY_IMPORT_MARKER


def _import_legacy_memory(
    stored: dict[str, HarnessEntry], state_file: Path | None, root: Path | None = None
) -> int:
    """Carry entries written before pages existed into the page store, once.

    The old store cannot keep its memory kind: a page is the truth now. But an
    upgrade must not read as a deletion, so the entries become pages first, the
    state file is backed up, and only then does `save()` stop writing that kind.
    Running twice is harmless: the marker stops the second pass, and a store that
    already has pages is left alone rather than merged into.
    """
    if not stored or _legacy_import_marker(root).exists() or memory_pages(root):
        return 0
    if state_file is not None and state_file.exists():
        backup = state_file.with_name(_LEGACY_BACKUP_NAME)
        try:
            backup.write_bytes(state_file.read_bytes())
        except OSError:
            # No backup means no permission to destroy the originals.
            return 0
    imported = 0
    for entry_id, entry in stored.items():
        metadata = entry.metadata if isinstance(entry.metadata, dict) else {}
        page_type = str(metadata.get("type") or _MEMORY_DEFAULT_TYPE)
        if page_type not in _MEMORY_TYPES:
            page_type = _MEMORY_DEFAULT_TYPE
        try:
            _write_memory_page(
                entry_id,
                page_type=page_type,
                title=entry.title,
                content=entry.content,
                hook=metadata.get("hook") or entry.title,
                status="active",
                root=root,
            )
        except (OSError, ValueError):
            continue
        imported += 1
    marker = _legacy_import_marker(root)
    try:
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(
            json.dumps(
                {
                    "imported": imported,
                    "of": len(stored),
                    "at": datetime.now(timezone.utc).isoformat(),
                }
            ),
            encoding="utf-8",
        )
    except OSError:
        return imported
    return imported


def memory_pages(root: Path | None = None) -> dict[str, HarnessEntry]:
    """Every knowledge page as an entry, for read tools that predate pages."""
    return {
        slug: _memory_page_entry(slug, path)
        for slug, path in sorted(_page_files(root).items())
    }


def _require_refinements_enabled() -> None:
    """Read the host's global-only master switch, without caching it."""
    try:
        document = json.loads((_agent_dir() / "settings.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return
    value = document.get("harnessRefinements") if isinstance(document, dict) else None
    if value is False:
        raise RuntimeError("Harness refinements are disabled in /settings")


def _resolve_global_flag(global_: bool = False, extra: dict[str, Any] | None = None) -> bool:
    extra = dict(extra or {})
    if "global" in extra:
        value = extra.pop("global")
        if not isinstance(value, bool):
            raise TypeError(f"global must be a bool, got {type(value).__name__}")
        global_ = value
    if extra:
        unexpected = next(iter(extra))
        raise TypeError(f"unexpected keyword argument {unexpected!r}")
    return bool(global_)


def _strip_scope_prefix(id: str | None, global_: bool) -> tuple[str | None, bool]:
    # overview() displays entries as [local:id]/[global:id]; accept those ids
    # verbatim. A global: prefix routes to the global store unless the caller
    # already forced a scope via global_.
    if isinstance(id, str):
        scope, sep, rest = id.partition(":")
        if sep and rest and scope in ("local", "global"):
            return rest, global_ or scope == "global"
    return id, global_


def _env_dir(name: str) -> str | None:
    # Set-but-empty env values must behave as unset; a bare "" would skip the
    # session-dir fallback and land local writes in the global agent-dir default.
    value = (os.environ.get(name) or "").strip()
    return value or None


def _state_file(state_dir: str | Path | None = None, *, global_: bool = False) -> Path:
    root: str | Path | None = state_dir
    if root is None:
        root = _env_dir("RLM_GLOBAL_HARNESS_STATE_DIR") if global_ else _env_dir("RLM_HARNESS_STATE_DIR")
    if root is None and not global_ and (session_dir := _env_dir("RLM_SESSION_DIR")):
        root = Path(session_dir) / _DEFAULT_HARNESS_DIR_NAME
    if root is None and not global_:
        raise RuntimeError(
            "Local harness state requires RLM_HARNESS_STATE_DIR or RLM_SESSION_DIR. "
            "Use get_harness_state(global_=True) for global state."
        )
    if root:
        return Path(root).expanduser().resolve() / _DEFAULT_FILE_NAME
    return _agent_dir() / _DEFAULT_HARNESS_DIR_NAME / _DEFAULT_FILE_NAME


@dataclass
class HarnessEntry:
    """A reusable prompt, memory, skill, or subagent record."""

    id: str
    kind: HarnessKind
    title: str
    content: str
    path: str = "general"
    scope: HarnessScope = "local"
    reference: dict[str, Any] = field(default_factory=dict)
    arguments: dict[str, Any] = field(default_factory=dict)
    metadata: dict[str, Any] = field(default_factory=dict)
    source: str = "agent"
    created_at: str = field(default_factory=_now)
    updated_at: str = field(default_factory=_now)
    version: int = 1


@dataclass
class RefinementEvent:
    """A recorded online harness-refinement pass."""

    id: str
    trigger: str
    changes: list[str]
    evidence: str = ""
    outcome: str = ""
    created_at: str = field(default_factory=_now)


_ENTRY_FIELDS = {field.name for field in fields(HarnessEntry)}
_REFINEMENT_FIELDS = {field.name for field in fields(RefinementEvent)}


def _validate_python_skill_reference(reference: dict[str, Any] | None, entry_name: str = "") -> dict[str, Any]:
    # Rejections name the entry so the caller can repair the right skill; the
    # suffix keeps the historical message text greppable.
    prefix = f"skill entry {entry_name!r} rejected: " if entry_name else ""

    def reject(message: str) -> None:
        raise ValueError(f"{prefix}{message}")

    if not isinstance(reference, dict):
        reject("skill entries require a Python reference")
    normalized = dict(reference)
    if normalized.get("type") != "python":
        reject("skill reference.type must be 'python'")
    if not any(isinstance(normalized.get(key), str) and normalized[key] for key in ("import", "python_import")):
        reject("skill reference requires a Python import")
    if not any(isinstance(normalized.get(key), str) and normalized[key] for key in ("callable", "call_pattern")):
        reject("skill reference requires a callable or call_pattern")
    return normalized


def _type_name(value: Any) -> str:
    if isinstance(value, list):
        return "a list"
    if value == "":
        return "an empty string"
    return type(value).__name__


def _describe_entry(id: Any, title: Any) -> str:
    """Best available entry name for rejection messages."""
    if isinstance(id, str) and id:
        return id
    if isinstance(title, str) and title:
        return title
    return "<unnamed>"


def _require_text(kind: str, entry_name: str, field: str, value: Any) -> None:
    if not isinstance(value, str) or not value:
        raise ValueError(
            f"{kind} entry {entry_name!r} rejected: {field} must be a non-empty string, got {_type_name(value)}"
        )


def _require_optional_text(kind: str, entry_name: str, field: str, value: Any) -> None:
    if value is not None:
        _require_text(kind, entry_name, field, value)


def _require_optional_record(kind: str, entry_name: str, field: str, value: Any) -> None:
    if value is not None and not isinstance(value, dict):
        raise ValueError(
            f"{kind} entry {entry_name!r} rejected: {field} must be a dict when provided, got {_type_name(value)}"
        )


def _factory_spec_argument(
    dag: Any, machine: Any
) -> "tuple[Any, Literal['dag', 'machine']]":
    """Pick the factory spec payload and its arguments key from the call.

    Supplying both forms at once is an error. A bare ``dag=None,
    machine=None`` passes ``None`` through in the dag slot so the write-time
    validation rejects it with the standard wording.
    """
    if dag is not None and machine is not None:
        raise ValueError("pass either dag or machine, not both")
    if machine is not None:
        return machine, "machine"
    return dag, "dag"


def _validate_factory_arguments(entry_name: str, arguments: dict[str, Any]) -> None:
    """Shared-path dry run for every factory write.

    ``create_factory``/``update_factory`` validate their own spec, but a
    generic ``create``/``update`` (or a refinement edit) writes
    ``arguments`` directly; an invalid spec must never reach the store
    through any writer, so the spec found in ``arguments`` is validated
    here too.
    """
    dag, machine = arguments.get("dag"), arguments.get("machine")
    if dag is not None and machine is not None:
        raise ValueError(f"factory entry {entry_name!r} rejected: pass either dag or machine, not both")
    spec = machine if machine is not None else dag
    if not isinstance(spec, dict):
        raise ValueError(
            f"factory entry {entry_name!r} rejected: factory entries require a dag or machine object in arguments"
        )
    errors = validate_factory_spec(spec)
    if errors:
        raise ValueError(f"factory entry {entry_name!r} rejected: {'; '.join(errors)}")


def _validate_entry_shape(
    kind: str,
    entry_id: Any,
    title: Any,
    content: Any,
    *,
    path: Any,
    reference: Any,
    arguments: Any,
    metadata: Any,
    source: Any,
    existing: "HarnessEntry | None",
) -> None:
    """Reject an invalid harness entry before anything is persisted.

    Every create/update/upsert write funnels through here, so a malformed
    entry (content as a list, title as a number) fails with an actionable
    error naming the entry and the field instead of being saved and later
    crashing the host digest that renders every session's system prompt.
    """
    entry_name = _describe_entry(entry_id, title)
    _require_text(kind, entry_name, "id", entry_id)
    _require_text(kind, entry_name, "title", title)
    _require_text(kind, entry_name, "content", content)
    _require_optional_text(kind, entry_name, "path", path)
    _require_optional_record(kind, entry_name, "reference", reference)
    _require_optional_record(kind, entry_name, "arguments", arguments)
    _require_optional_record(kind, entry_name, "metadata", metadata)
    _require_text(kind, entry_name, "source", source)
    if kind == "skill":
        if reference is None:
            # A new skill without a Python reference is invalid; an update that
            # omits it preserves the existing reference instead.
            if existing is None:
                raise ValueError(f"skill entry {entry_name!r} rejected: skill entries require a Python reference")
        else:
            _validate_python_skill_reference(reference, entry_name)
    if kind == "factory":
        # Every factory writer funnels through here, so the opt-in gate and
        # the spec dry run cover them all: create_factory validates, and the
        # generic create/update path (a refinement edit) gets the same
        # treatment. The gate comes first: while `factory.enabled` is off
        # (the default) every factory write refuses with the one disabled
        # message, before any spec work. A
        # NEW factory requires its spec (an arguments-less factory would
        # store an unusable entry that run() later rejects); an update that
        # omits arguments (None) preserves the stored spec and skips
        # validation, exactly like update_skill treats reference.
        require_factory_enabled()
        if arguments is None and existing is None:
            raise ValueError(
                f"factory entry {entry_name!r} rejected: factory entries require a dag or machine object in arguments"
            )
        if arguments is not None:
            _validate_factory_arguments(entry_name, arguments)


def _validate_refinement_event(trigger: Any, changes: Any, *, evidence: Any, outcome: Any) -> None:
    """Reject a refinement event whose persisted shape would break the digest."""
    if not isinstance(trigger, str) or not trigger:
        raise ValueError(f"refinement event rejected: trigger must be a non-empty string, got {_type_name(trigger)}")
    if isinstance(changes, str):
        if not changes:
            raise ValueError("refinement event rejected: changes must be a non-empty string or a list of strings")
    elif isinstance(changes, list):
        if not all(isinstance(change, str) and change for change in changes):
            raise ValueError("refinement event rejected: changes must be a list of non-empty strings")
    else:
        raise ValueError(
            f"refinement event rejected: changes must be a string or a list of strings, got {_type_name(changes)}"
        )
    if not isinstance(evidence, str):
        raise ValueError(f"refinement event rejected: evidence must be a string when provided, got {_type_name(evidence)}")
    if not isinstance(outcome, str):
        raise ValueError(f"refinement event rejected: outcome must be a string when provided, got {_type_name(outcome)}")


class HarnessState:
    """CRUD store for reset-free harness refinement state."""

    def __init__(
        self,
        file_path: str | Path | None = None,
        *,
        in_memory: bool = False,
        scope: HarnessScope = "local",
        local_write_error: str | None = None,
    ):
        # in_memory mode never resolves or touches a path. It is the safe fallback when
        # path resolution itself fails, so constructing it cannot re-raise that error.
        if in_memory:
            self.file_path: Path | None = None
        else:
            self.file_path = (
                Path(file_path).expanduser().resolve()
                if file_path
                else _state_file(global_=(scope == "global"))
            )
        self.scope: HarnessScope = scope
        # When set, local mutations raise instead of vanishing into a volatile
        # store; reads and global_=True delegation keep working.
        self._local_write_error = local_write_error
        self.entries: dict[HarnessKind, dict[str, HarnessEntry]] = {kind: {} for kind in _KINDS}
        # Entries the page store has not taken over yet: written back on save.
        self._legacy_memory: dict[str, HarnessEntry] = {}
        self.refinements: list[RefinementEvent] = []
        self._global_target_state_dir: Path | None = None
        # mtime of the file as of the last load/save, used to detect out-of-process
        # writes (e.g. the host `/refine` command) and avoid clobbering them.
        self._loaded_mtime: int | None = None
        self._held_lock: tuple[Path, str] | None = None
        # The proxy's empty fallback must remain constructible for kernel API
        # introspection while disabled; every actual read/write still checks the gate.
        if not in_memory:
            self.load()

    @property
    def _memory_root(self) -> Path:
        """Where this state's pages live: beside the state, not in the process env."""
        return _memory_root_for(self.file_path)

    def _require_page_store(self) -> None:
        """An in-memory state has no page store, and must not borrow the env one."""
        if self.file_path is None:
            raise RuntimeError(
                "memory pages need an agent directory: this state is in memory only"
            )

    def _sync_page_backed(self) -> None:
        """Re-read the page-backed kinds: their truth is on disk, not in this dict."""
        for kind in _PAGE_BACKED_KINDS:
            self.entries[kind] = memory_pages(self._memory_root)

    def _route_page_backed(self, kind: HarnessKind) -> bool:
        """Whether this kind is handled by the page store rather than the entry map."""
        return kind in _PAGE_BACKED_KINDS

    def _ensure_local_writable(self) -> None:
        _require_refinements_enabled()
        if self._local_write_error is not None:
            raise RuntimeError(self._local_write_error)

    def _disk_mtime(self) -> int | None:
        if self.file_path is None:
            return None
        try:
            return self.file_path.stat().st_mtime_ns
        except OSError:
            return None

    def _sync_from_disk(self) -> None:
        """Reload if another process rewrote the state file since we last touched it.

        The kernel keeps a long-lived ``HarnessState`` in memory while the host
        ``/refine`` command rewrites the same file from a separate process. Without
        this guard the next in-kernel ``save()`` would overwrite host edits with a
        stale snapshot. We re-read whenever the on-disk mtime no longer matches the
        value recorded at our last load/save.
        """
        _require_refinements_enabled()
        if self._disk_mtime() != self._loaded_mtime:
            self.load()

    @staticmethod
    def _owner_dead(recorded: str) -> bool:
        parts = recorded.strip().split(" ")
        if len(parts) != 2 or not parts[1]:
            return True
        try:
            pid = int(parts[0])
        except ValueError:
            return True
        if pid <= 0:
            return True
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        except (PermissionError, OSError):
            return False
        return False

    @staticmethod
    def _owner_matches(lock_path: Path, owner: str) -> bool:
        try:
            return (lock_path / "owner").read_text(encoding="utf-8").strip() == owner
        except OSError:
            return False

    def _acquire_lock_dir(self, lock_path: Path) -> str:
        owner = f"{os.getpid()} {_PROCESS_TOKEN}.{uuid4().hex}"
        owner_path = lock_path / "owner"
        for _ in range(_LOCK_ATTEMPTS):
            try:
                os.mkdir(lock_path)
                try:
                    os.chmod(lock_path, 0o700)
                    owner_path.write_text(f"{owner}\n", encoding="utf-8")
                    os.chmod(owner_path, 0o600)
                except OSError:
                    lock_path.rmdir()
                    raise
                return owner
            except FileExistsError:
                try:
                    stale = time.time() - lock_path.stat().st_mtime > _LOCK_STALE_AFTER
                except FileNotFoundError:
                    continue
                if stale:
                    try:
                        recorded = owner_path.read_text(encoding="utf-8")
                    except (OSError, UnicodeError):
                        recorded = None
                    if recorded is None or self._owner_dead(recorded):
                        with contextlib.suppress(FileNotFoundError):
                            owner_path.unlink()
                        with contextlib.suppress(FileNotFoundError):
                            lock_path.rmdir()
                        continue
                time.sleep(_LOCK_RETRY_INTERVAL)
        raise RuntimeError(f"harness state lock not acquired: {lock_path}")

    @contextlib.contextmanager
    def _locked(self) -> Iterator[None]:
        if self.file_path is not None:
            target = Path(os.path.realpath(self.file_path))
            target.parent.mkdir(parents=True, exist_ok=True)
            lock_path = target.with_name(f"{target.name}.lock")
            self._held_lock = (lock_path, self._acquire_lock_dir(lock_path))
        try:
            self._sync_from_disk()
            yield
        finally:
            if self._held_lock is not None:
                lock_path, owner = self._held_lock
                if self._owner_matches(lock_path, owner):
                    (lock_path / "owner").unlink()
                    lock_path.rmdir()
                self._held_lock = None

    def load(self) -> "HarnessState":
        _require_refinements_enabled()
        if self.file_path is None or not self.file_path.exists():
            self._loaded_mtime = None
            return self
        mtime = self._disk_mtime()
        try:
            with self.file_path.open("r", encoding="utf-8") as f:
                data = json.load(f)
        except (OSError, ValueError):
            # A corrupt or unreadable state file must not crash the kernel or block
            # refinement. Treat it as empty; the next save() rewrites it cleanly.
            data = {}
        # json.load returns non-dict types for valid JSON like `null`, `[]`, or a bare
        # string; coerce those to an empty object before attribute access.
        if not isinstance(data, dict):
            data = {}

        entries: dict[HarnessKind, dict[str, HarnessEntry]] = {kind: {} for kind in _KINDS}
        raw_entries = data.get("entries", {})
        if isinstance(raw_entries, dict):
            for kind in _KINDS:
                raw_kind_entries = raw_entries.get(kind, {})
                if not isinstance(raw_kind_entries, dict):
                    continue
                for entry_id, raw_entry in raw_kind_entries.items():
                    if isinstance(raw_entry, dict):
                        entry_data = {key: value for key, value in raw_entry.items() if key in _ENTRY_FIELDS}
                        entry_data["id"] = str(entry_id)
                        entry_data["kind"] = kind
                        if not isinstance(entry_data.get("title"), str) or not isinstance(
                            entry_data.get("content"), str
                        ):
                            continue
                        if not isinstance(entry_data.get("path"), str):
                            entry_data["path"] = "general"
                        if entry_data.get("scope") not in ("local", "global"):
                            entry_data["scope"] = self.scope
                        if not isinstance(entry_data.get("source"), str):
                            entry_data["source"] = "agent"
                        version = entry_data.get("version", 1)
                        if isinstance(version, str):
                            try:
                                version = int(version)
                            except ValueError:
                                version = 1
                        if not isinstance(version, int):
                            version = 1
                        entry_data["version"] = version
                        if not isinstance(entry_data.get("reference"), dict):
                            entry_data["reference"] = {}
                        if not isinstance(entry_data.get("arguments"), dict):
                            entry_data["arguments"] = {}
                        if not isinstance(entry_data.get("metadata"), dict):
                            entry_data["metadata"] = {}
                        entries[kind][str(entry_id)] = HarnessEntry(**entry_data)
        # Memory is page-backed: the projection replaces whatever the entry store
        # holds for that kind, so every reader of `entries` sees the pages. What
        # the store still holds is carried over first, and kept untouched when it
        # cannot be — a lost import must not read as a lost memory.
        stored_memory = dict(entries["memory"])
        if self.file_path is not None:
            _import_legacy_memory(stored_memory, self.file_path, self._memory_root)
        self._legacy_memory = (
            {} if _legacy_import_marker(self._memory_root).exists() else stored_memory
        )
        entries["memory"] = memory_pages(self._memory_root)
        self.entries = entries

        self.refinements = []
        raw_refinements = data.get("refinements", [])
        if isinstance(raw_refinements, list):
            for raw_event in raw_refinements:
                if isinstance(raw_event, dict):
                    event_data = {key: value for key, value in raw_event.items() if key in _REFINEMENT_FIELDS}
                    if not isinstance(event_data.get("id"), str) or not isinstance(
                        event_data.get("trigger"), str
                    ):
                        continue
                    changes = event_data.get("changes")
                    if isinstance(changes, str):
                        event_data["changes"] = [changes]
                    elif isinstance(changes, list):
                        event_data["changes"] = [str(change) for change in changes]
                    elif not isinstance(changes, list):
                        continue
                    self.refinements.append(RefinementEvent(**event_data))
        self._loaded_mtime = mtime
        return self

    def _global_target(self, global_: bool, extra: dict[str, Any] | None = None) -> "HarnessState | None":
        if not _resolve_global_flag(global_, extra):
            return None
        target = get_harness_state(state_dir=self._global_target_state_dir, global_=True)
        if self.file_path is not None and target.file_path == self.file_path and target.scope == self.scope:
            return None
        return target

    def save(self) -> "HarnessState":
        _require_refinements_enabled()
        if self.file_path is None:
            # in_memory fallback: nothing to persist.
            return self
        if self._held_lock is None:
            raise RuntimeError("harness state lock not acquired")
        self.file_path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            "schema": 1,
            # A page-backed kind is not written here once its pages are: its pages
            # are the store, and persisting the projection would hand the next load
            # a stale duplicate. Until the import has happened, the entries the
            # file already holds are written back verbatim, not dropped.
            "entries": {
                kind: {
                    entry_id: asdict(entry)
                    for entry_id, entry in (
                        self._legacy_memory.items()
                        if kind in _PAGE_BACKED_KINDS
                        else records.items()
                    )
                }
                for kind, records in self.entries.items()
            },
            "refinements": [asdict(event) for event in self.refinements],
        }
        # Atomic replace on the real file: aliases survive, readers never see a torn file.
        target_path = Path(os.path.realpath(self.file_path))
        temp_path = target_path.with_name(f"{target_path.name}.{os.getpid()}.{uuid4().hex}.tmp")
        try:
            existing_mode = stat.S_IMODE(os.stat(target_path).st_mode)
        except FileNotFoundError:
            existing_mode = None
        mode = existing_mode if existing_mode is not None else 0o600
        try:
            # Create no looser than the destination; retain the umask for new files.
            descriptor = os.open(temp_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
            with os.fdopen(descriptor, "w", encoding="utf-8") as f:
                json.dump(data, f, indent=2, ensure_ascii=False)
                f.flush()
                if sys.platform == "darwin":
                    fcntl.fcntl(descriptor, fcntl.F_FULLFSYNC)
                else:
                    os.fsync(descriptor)
            if existing_mode is not None:
                os.chmod(temp_path, existing_mode)
            lock_path, owner = self._held_lock
            if not self._owner_matches(lock_path, owner):
                raise RuntimeError(f"harness state lock lost: {lock_path}")
            os.replace(temp_path, target_path)
            if os.name == "posix":
                directory_fd = os.open(target_path.parent, os.O_RDONLY)
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
        finally:
            temp_path.unlink(missing_ok=True)
        self._loaded_mtime = self._disk_mtime()
        return self

    def upsert(
        self,
        kind: HarnessKind,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = "agent",
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.upsert(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        self._ensure_local_writable()
        with self._locked():
            return self._upsert(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )

    def _upsert(
        self,
        kind: HarnessKind,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str | None = None,
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = "agent",
    ) -> HarnessEntry:
        # Caller is responsible for syncing from disk first. create()/update() sync
        # once and then call this directly so their existence check and the write are
        # not separated by a second reload (which could turn create-or-fail into a
        # silent update).
        if kind not in self.entries:
            raise ValueError(f"unknown harness kind {kind!r}; expected one of {_KINDS}")

        # Guard before the id slug and the dict lookup: a non-string title or a
        # non-string id (falsy ids included, which the slug fallback would
        # silently collapse) must fail with a clear rejection, not an
        # AttributeError inside slug normalization or a TypeError from the lookup.
        _require_text(kind, _describe_entry(id, title), "title", title)
        if id is not None:
            _require_text(kind, _describe_entry(id, title), "id", id)
        entry_id = id or _slug(title, kind)
        existing = self.entries[kind].get(entry_id)
        _validate_entry_shape(
            kind,
            entry_id,
            title,
            content,
            path=path,
            reference=reference,
            arguments=arguments,
            metadata=metadata,
            source=source,
            existing=existing,
        )
        if existing:
            existing.title = title
            existing.content = content
            # Preserve path/reference/arguments/metadata when the caller omits them
            # (None) so updating only an entry's title or content does not reset its
            # grouping path or wipe a skill's reference/argument contract. An explicit
            # value (including {}) still overwrites.
            if path is not None:
                existing.path = path
            if reference is not None:
                existing.reference = dict(reference)
            if arguments is not None:
                # Factory specs are deep-copied: the nested dag/machine object
                # is caller-owned, and a later mutation must never change the
                # stored (validated) spec without a write-time dry run.
                existing.arguments = copy.deepcopy(arguments) if kind == "factory" else dict(arguments)
            if metadata is not None:
                existing.metadata = dict(metadata)
            existing.source = source
            existing.updated_at = _now()
            existing.version += 1
            entry = existing
        else:
            entry = HarnessEntry(
                id=entry_id,
                kind=kind,
                title=title,
                content=content,
                path=path if path is not None else "general",
                scope=self.scope,
                reference=dict(reference or {}),
                arguments=copy.deepcopy(arguments or {}) if kind == "factory" else dict(arguments or {}),
                metadata=dict(metadata or {}),
                source=source,
            )
            self.entries[kind][entry_id] = entry
        self.save()
        return entry

    def get(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Any) -> HarnessEntry | None:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.get(kind, id)
        self._sync_from_disk()
        self._sync_page_backed()
        if kind not in self.entries:
            raise ValueError(f"unknown harness kind {kind!r}; expected one of {_KINDS}")
        return self.entries[kind].get(id)

    def delete(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.delete(kind, id)
        if self._route_page_backed(kind):
            self._require_page_store()
            return self.delete_memory(id, global_=global_)
        self._ensure_local_writable()
        with self._locked():
            if kind not in self.entries:
                raise ValueError(f"unknown harness kind {kind!r}; expected one of {_KINDS}")
            if id not in self.entries[kind]:
                return False
            del self.entries[kind][id]
            self.save()
            return True

    def list(self, kind: HarnessKind | None = None, *, global_: bool = False, **kwargs: Any) -> list[HarnessEntry]:
        if target := self._global_target(global_, kwargs):
            return target.list(kind)
        self._sync_from_disk()
        self._sync_page_backed()
        kinds = [kind] if kind else list(_KINDS)
        records: list[HarnessEntry] = []
        for current_kind in kinds:
            if current_kind not in self.entries:
                raise ValueError(f"unknown harness kind {current_kind!r}; expected one of {_KINDS}")
            records.extend(self.entries[current_kind].values())
        return sorted(records, key=lambda entry: (entry.kind, entry.path, entry.title, entry.id))

    def create(
        self,
        kind: HarnessKind,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = "agent",
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.create(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        self._ensure_local_writable()
        if self._route_page_backed(kind):
            return self.create_memory(
                title,
                content,
                id=id,
                path=path,
                metadata=metadata,
                global_=global_,
            )
        with self._locked():
            if kind not in self.entries:
                raise ValueError(f"unknown harness kind {kind!r}; expected one of {_KINDS}")
            _require_text(kind, _describe_entry(id, title), "title", title)
            if id is not None:
                _require_text(kind, _describe_entry(id, title), "id", id)
            entry_id = id or _slug(title, kind)
            if entry_id in self.entries[kind]:
                raise ValueError(f"{kind} entry {entry_id!r} already exists")
            return self._upsert(
                kind,
                title,
                content,
                id=entry_id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )

    def update(
        self,
        kind: HarnessKind,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = "agent",
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.update(
                kind,
                id,
                title,
                content,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        self._ensure_local_writable()
        if self._route_page_backed(kind):
            self._require_page_store()
            return self.update_memory(
                id,
                title,
                content,
                path=path,
                metadata=metadata,
                global_=global_,
            )
        with self._locked():
            if kind not in self.entries:
                raise ValueError(f"unknown harness kind {kind!r}; expected one of {_KINDS}")
            _require_text(kind, _describe_entry(id, title), "id", id)
            if id not in self.entries[kind]:
                raise ValueError(f"{kind} entry {id!r} does not exist")
            return self._upsert(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )

    def create_memory(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        metadata: dict[str, Any] | None = None,
        global_: bool = True,
        type: str | None = None,
        hook: str | None = None,
        **kwargs: Any,
    ) -> HarnessEntry:
        """Create a memory page.

        Memory is page-backed: this writes a markdown file under
        ``<agent dir>/memory/pages/<type>/`` and returns the projected entry.
        ``type`` decides how long the page stays worth reading and defaults to
        ``experience`` (one session's record); ``hook`` is the routing line the
        digest shows. The digest index picks the page up on the next render.

        Pages are global: there is one store, shared by every session. A caller
        asking for a session-local memory is refused rather than quietly given a
        global one — a scope that silently widens is worse than an error.
        """
        _refuse_local_memory(global_)
        _resolve_global_flag(global_, kwargs)
        self._ensure_local_writable()
        self._require_page_store()
        _require_text("memory", title, "title", title)
        slug = id or _slug(title, "memory")
        _require_text("memory", _describe_entry(slug, title), "id", slug)
        page_type = type or (
            metadata.get("type") if isinstance(metadata, dict) and metadata.get("type") else None
        ) or _MEMORY_DEFAULT_TYPE
        page_hook = hook or (
            metadata.get("hook") if isinstance(metadata, dict) and metadata.get("hook") else None
        )
        page = _write_memory_page(
            slug,
            page_type=page_type,
            title=title,
            content=content,
            hook=page_hook,
            status="draft",
            merge_path=_page_files(self._memory_root).get(slug),
            root=self._memory_root,
        )
        return _memory_page_entry(slug, page)

    def update_memory(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        hook: str | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        _resolve_global_flag(global_, kwargs)
        self._ensure_local_writable()
        self._require_page_store()
        pages = _page_files(self._memory_root)
        page = pages.get(id)
        if page is None:
            raise KeyError(f"memory page {id!r} not found under {_memory_pages_dir(self._memory_root)}")
        page_type = (metadata or {}).get("type") or page.parent.name or _MEMORY_DEFAULT_TYPE
        if page_type not in _MEMORY_TYPES:
            page_type = _MEMORY_DEFAULT_TYPE
        written = _write_memory_page(
            id,
            page_type=page_type,
            title=title,
            content=content,
            hook=hook,
            status="active",
            merge_path=page,
            root=self._memory_root,
        )
        # A type change moves the page to another directory; leaving the old copy
        # behind would give one slug two pages and a silent winner by rglob order.
        if written != page:
            with contextlib.suppress(OSError):
                page.unlink()
        return _memory_page_entry(id, written)

    def delete_memory(self, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        """Archive a memory page instead of unlinking it.

        Removal is dream's decision, not the agent's: this stops a page from
        reaching the index (it moves under ``memory/archive/``) and leaves a
        tombstone line in ``memory/_archive.md``, so a mistaken archive is
        reversible and the next consolidation pass does not recreate the page.
        """
        _resolve_global_flag(global_, kwargs)
        self._ensure_local_writable()
        self._require_page_store()
        refusal = _removal_refusal(id, self._memory_root)
        if refusal is not None:
            raise ValueError(f"memory page {id!r} cannot be removed: {refusal}")
        return _tombstone_memory_page(id, root=self._memory_root)

    def record_dream_pass(self, **kwargs: Any) -> dict[str, Any]:
        """Record a finished consolidation pass; see `record_dream_pass`."""
        _require_refinements_enabled()
        return record_dream_pass(root=self._memory_root, **kwargs)

    def dream_due(self) -> tuple[bool, str]:
        """Whether a consolidation pass is due, from the journal."""
        return dream_due(self._memory_root)

    def merge_pages(self, keep: str, absorb: str) -> bool:
        """Merge the page `absorb` into `keep`; false when either page is gone."""
        _require_refinements_enabled()
        return merge_pages(keep, absorb, self._memory_root)

    def create_prompt_note(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "policy",
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        return self.create("prompt", title, content, id=id, path=path, metadata=metadata, global_=global_, **kwargs)

    def update_prompt_note(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        return self.update("prompt", id, title, content, path=path, metadata=metadata, global_=global_, **kwargs)

    def delete_prompt_note(self, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        return self.delete("prompt", id, global_=global_, **kwargs)

    def create_skill(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        return self.create(
            "skill",
            title,
            content,
            id=id,
            path=path,
            reference=_validate_python_skill_reference(reference, _describe_entry(id, title)),
            arguments=arguments,
            metadata=metadata,
            global_=global_,
            **kwargs,
        )

    def update_skill(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        # Only validate a reference when one is supplied; omitting it preserves the
        # existing reference (see _upsert) rather than forcing every title/content-only
        # update to re-send the full Python reference.
        validated_reference = (
            _validate_python_skill_reference(reference, _describe_entry(id, title)) if reference is not None else None
        )
        return self.update(
            "skill",
            id,
            title,
            content,
            path=path,
            reference=validated_reference,
            arguments=arguments,
            metadata=metadata,
            global_=global_,
            **kwargs,
        )

    def delete_skill(self, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        return self.delete("skill", id, global_=global_, **kwargs)

    def create_subagent(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        return self.create("subagent", title, content, id=id, path=path, metadata=metadata, global_=global_, **kwargs)

    def update_subagent(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        return self.update("subagent", id, title, content, path=path, metadata=metadata, global_=global_, **kwargs)

    def delete_subagent(self, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        return self.delete("subagent", id, global_=global_, **kwargs)

    def create_factory(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        dag: dict[str, Any] | None = None,
        machine: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        # The opt-in gate precedes the dry run, so a disabled factory
        # refuses with the one disabled message whatever the spec looks like.
        require_factory_enabled()
        # Write-time dry run: an invalid spec (either form) never reaches the
        # store. The spec is deep-copied before storing: mutating the caller's
        # dict after creation must not change the live entry (a later update
        # that omits both forms would preserve the mutated, unvalidated spec).
        spec, key = _factory_spec_argument(dag, machine)
        errors = validate_factory_spec(spec)
        if errors:
            raise ValueError("; ".join(errors))
        return self.create(
            "factory",
            title,
            content,
            id=id,
            path=path,
            arguments={key: copy.deepcopy(spec)},
            metadata=metadata,
            global_=global_,
            **kwargs,
        )

    def update_factory(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        dag: dict[str, Any] | None = None,
        machine: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> HarnessEntry:
        # The opt-in gate precedes the spec and existence checks, so a
        # disabled factory refuses with the one disabled message whatever the
        # update carries.
        require_factory_enabled()
        # Only validate a spec when one is supplied; omitting both preserves the
        # stored arguments (see _upsert) rather than forcing every title/content
        # update to re-send the full spec, exactly like update_skill treats reference.
        if dag is not None or machine is not None:
            spec, key = _factory_spec_argument(dag, machine)
            errors = validate_factory_spec(spec)
            if errors:
                raise ValueError("; ".join(errors))
            arguments = {key: copy.deepcopy(spec)}
        else:
            arguments = None
        return self.update(
            "factory",
            id,
            title,
            content,
            path=path,
            arguments=arguments,
            metadata=metadata,
            global_=global_,
            **kwargs,
        )

    def delete_factory(self, id: str, *, global_: bool = False, **kwargs: Any) -> bool:
        return self.delete("factory", id, global_=global_, **kwargs)

    def record_refinement(
        self,
        trigger: str,
        changes: list[str] | str,
        *,
        evidence: str = "",
        outcome: str = "",
        id: str | None = None,
        global_: bool = False,
        **kwargs: Any,
    ) -> RefinementEvent:
        if target := self._global_target(global_, kwargs):
            return target.record_refinement(trigger, changes, evidence=evidence, outcome=outcome, id=id)
        self._ensure_local_writable()
        with self._locked():
            _validate_refinement_event(trigger, changes, evidence=evidence, outcome=outcome)
            if id is not None and (not isinstance(id, str) or not id):
                raise ValueError(
                    f"refinement event rejected: id must be a non-empty string when provided, got {_type_name(id)}"
                )
            event_id = id or f"refine_{len(self.refinements) + 1:04d}"
            normalized_changes = [changes] if isinstance(changes, str) else list(changes)
            event = RefinementEvent(
                id=event_id,
                trigger=trigger,
                changes=normalized_changes,
                evidence=evidence,
                outcome=outcome,
            )
            self.refinements.append(event)
            self.save()
            return event

    def plan_refinement(
        self,
        observation: str,
        *,
        failing_component: str = "",
        next_step: str = "",
    ) -> list[str]:
        target = f" for {failing_component}" if failing_component else ""
        plan = [
            f"Diagnose the repeated failure or opportunity{target}: {observation}",
            "Update the smallest useful prompt note, memory item, skill, or subagent spec.",
            "Run the next action with the changed harness state, then record the outcome.",
        ]
        if next_step:
            plan.append(f"Immediate validation step: {next_step}")
        return plan

    def overview(self, *, max_entries_per_kind: int = 20, global_: bool = False, **kwargs: Any) -> str:
        if target := self._global_target(global_, kwargs):
            return target.overview(max_entries_per_kind=max_entries_per_kind)
        self._sync_from_disk()
        lines = [
            f"Harness state ({self.scope}): {self.file_path}",
            "Call contract: installed Python skills use await <skill_import>(...) or a matching shell CLI; "
            "harness skill entries are Python REPL skills and must include a Python reference plus arguments. "
            "Spawn a subagent spec by composing a concise task prompt and calling "
            "handle = await rlm.spawn('sub-task', name='worker'); admission returns immediately with rlm_child_id, name, session_dir, "
            "and model, never the child's answer. Results arrive only through explicit agent_message replies or "
            "files; children reply with await agent_message.send(message, receiver_role='parent'). Use "
            "await rlm.list_subagents() to recover direct child handles and await agent_message.send(..., "
            "receiver_role='child', receiver_name=handle.name) for follow-ups.",
            "Factory entries declare validated state-machine workflows of subagent states in arguments['machine'] "
            "(the original DAG sugar in arguments['dag'] compiles to machine form): manage them with "
            "create_factory/update_factory/delete_factory (create_factory validates either form at write time); run "
            "them with await rlm.factory.run(\"<id>\"), watch with await rlm.factory.status(run_id), stop with "
            "await rlm.factory.stop(run_id), and resume an escalate-paused run with "
            "await rlm.factory.resume(run_id).",
        ]
        for kind in _KINDS:
            records = self.list(kind)[:max_entries_per_kind]
            if kind == "memory":
                # Memory is page-backed: the count and the records come from the
                # pages on disk, not from this store's entries.
                pages = sorted(memory_pages(self._memory_root).values(), key=lambda entry: entry.id)
                lines.append(f"{kind}: {len(pages)} (page-backed, <agent dir>/memory/pages/)")
                for entry in pages[:max_entries_per_kind]:
                    fields = entry.metadata or {}
                    hook = fields.get("hook") or ""
                    lines.append(
                        f"  - [{entry.path}:{entry.id}] {entry.title}"
                        + (f" — read when: {hook}" if hook else "")
                    )
                overflow = len(pages) - max_entries_per_kind
                if overflow > 0:
                    lines.append(f"  - +{overflow} more")
                continue
            lines.append(f"{kind}: {len(self.entries[kind])}")
            for entry in records:
                summary = entry.content.strip().replace("\n", " ")
                if len(summary) > 120:
                    summary = f"{summary[:117]}..."
                argument_summary = ""
                if entry.kind == "skill" and entry.arguments:
                    argument_text = json.dumps(entry.arguments, ensure_ascii=False, sort_keys=True)
                    if len(argument_text) > 120:
                        argument_text = f"{argument_text[:117]}..."
                    argument_summary = f" args={argument_text}"
                reference_summary = ""
                if entry.kind == "skill" and entry.reference:
                    reference_text = json.dumps(entry.reference, ensure_ascii=False, sort_keys=True)
                    if len(reference_text) > 120:
                        reference_text = f"{reference_text[:117]}..."
                    reference_summary = f" ref={reference_text}"
                lines.append(
                    f"  - [{entry.scope}:{entry.id}] {entry.title} ({entry.path}, v{entry.version})"
                    f"{reference_summary}{argument_summary}: {summary}"
                )
            overflow = len(self.entries[kind]) - len(records)
            if overflow > 0:
                lines.append(f"  - +{overflow} more")
        if self.refinements:
            lines.append(f"refinements: {len(self.refinements)}")
            for event in self.refinements[-5:]:
                lines.append(f"  - [{event.id}] {event.trigger}: {', '.join(event.changes)}")
        else:
            lines.append("refinements: 0")
        return "\n".join(lines)

    def search(
        self,
        query: str,
        kind: HarnessKind | None = None,
        limit: int = 10,
        *,
        global_: bool = False,
        **kwargs: Any,
    ) -> list[HarnessEntry]:
        """Return harness entries ranked by weighted term overlap with *query*.

        Terms are scored against an entry's title, content, path, and id;
        matches in more distinct fields count more. Each matched term is
        discounted by its document frequency across the ranked corpus
        (tf-idf style, ``weight * log(1 + N / df)``), so a rare,
        distinctive term outranks terms present in most entries.
        """
        if target := self._global_target(global_, kwargs):
            return target.search(query, kind=kind, limit=limit)
        self._sync_from_disk()
        self._sync_page_backed()
        if not isinstance(query, str):
            raise TypeError(f"query must be str, got {type(query).__name__}")
        if not isinstance(limit, int) or isinstance(limit, bool) or limit < 1:
            raise TypeError("limit must be a positive int")
        terms = _harness_query_terms(query)
        if not terms:
            # Silence here is what produced duplicate memories: a query with no
            # usable term (ASCII shorter than three characters, punctuation only)
            # must say so instead of looking like "nothing matched".
            raise ValueError(
                f"query {query!r} has no searchable term: CJK characters count from one, "
                "other words need at least three characters"
            )

        entries = self.list(kind, **kwargs) if kind is not None else self.list(None, **kwargs)
        if kind is None or kind == "memory":
            # Pages are the memory store: they join the ranked corpus so a search
            # for a fact finds the page, not an empty legacy entry list.
            entries = [entry for entry in entries if entry.kind != "memory"]
            entries.extend(memory_pages(self._memory_root).values())

        # Document frequency per term over the ranked corpus: a term in
        # every entry weighs log(2), a term in one entry of N weighs
        # log(1 + N), so rare distinctive terms outrank ubiquitous ones.
        matches: dict[str, int] = {term: 0 for term in terms}
        for entry in entries:
            title = entry.title.lower()
            content = entry.content.lower()
            path_and_id = f"{entry.path} {entry.id}".lower()
            for term in terms:
                if term in title or term in content or term in path_and_id:
                    matches[term] += 1
        term_idf = {
            term: math.log(1 + len(entries) / count)
            for term, count in matches.items()
            if count > 0
        }

        def score(entry: HarnessEntry) -> float:
            title = entry.title.lower()
            content = entry.content.lower()
            path_and_id = f"{entry.path} {entry.id}".lower()
            total = 0.0
            for term, idf in term_idf.items():
                fields = (1 if term in title else 0) + (1 if term in content else 0) + (
                    1 if term in path_and_id else 0
                )
                if fields:
                    total += idf * (1 + (fields - 1) * 0.5)
            return total

        def recency(entry: HarnessEntry) -> str:
            return entry.updated_at if isinstance(entry.updated_at, str) else ""

        ranked = sorted(entries, key=lambda e: (score(e), recency(e)), reverse=True)
        ranked = [e for e in ranked if score(e) > 0]
        hits = ranked[:limit]
        for entry in hits:
            if entry.kind == "memory":
                _record_page_read(entry)
        return hits

    def snapshot(self, *, global_: bool = False, **kwargs: Any) -> dict[str, Any]:
        if target := self._global_target(global_, kwargs):
            return target.snapshot()
        self._sync_from_disk()
        self._sync_page_backed()
        return {
            "file_path": str(self.file_path),
            "scope": self.scope,
            "entries": {
                kind: {entry_id: asdict(entry) for entry_id, entry in records.items()}
                for kind, records in self.entries.items()
            },
            "refinements": [asdict(event) for event in self.refinements],
        }


def get_harness_state(
    state_dir: str | Path | None = None, *, global_: bool = False, **kwargs: Any
) -> HarnessState:
    """Return the cached local harness state, or global when requested."""
    _require_refinements_enabled()
    global_ = _resolve_global_flag(global_, kwargs)
    file_path = _state_file(state_dir, global_=global_)
    scope: HarnessScope = "global" if global_ else "local"
    cache_key = (file_path, scope)
    state = _state_cache.get(cache_key)
    if state is None:
        state = HarnessState(file_path, scope=scope)
        # Recorded at construction only: an instance created from env defaults must
        # keep targeting RLM_GLOBAL_HARNESS_STATE_DIR even when a later explicit
        # state_dir call aliases the same local file. An explicit dir that merely
        # aliases the env resolution must not sandbox later global_=True writes
        # either, so pin only when the explicit dir actually diverges.
        if state_dir is not None:
            try:
                env_file: Path | None = _state_file(global_=global_)
            except RuntimeError:
                env_file = None
            if file_path != env_file:
                state._global_target_state_dir = Path(state_dir).expanduser().resolve()
        _state_cache[cache_key] = state
    return state


__all__ = [
    "HarnessEntry",
    "HarnessKind",
    "HarnessScope",
    "HarnessState",
    "RefinementEvent",
    "get_harness_state",
]
