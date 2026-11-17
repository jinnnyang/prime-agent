//! Page-backed memory: pages are the source of truth, the resident index is a
//! render of them.
//!
//! One directory of markdown files per agent (`<agent dir>/memory/pages/<type>/`)
//! plus a generated index (`MEMORY.md`). The digest renders the index; the model
//! reads a page body with its normal file tools, so the index only has to make a
//! page findable, not restate it.

use std::path::{Path, PathBuf};

pub mod dream;

use serde::{Deserialize, Serialize};

/// Knowledge types a page may declare, in index order. Anything else lands in
/// `other`, which renders last and truncates first.
pub const MEMORY_TYPES: [&str; 4] = ["world", "model", "observation", "experience"];

/// Directory name under the agent dir that holds the pages.
pub const MEMORY_DIR_NAME: &str = "memory";
/// Subdirectory that holds the knowledge pages.
pub const PAGES_DIR_NAME: &str = "pages";

/// One page's index-relevant frontmatter. The body is deliberately not read:
/// the index routes, it does not summarize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryPage {
    pub slug: String,
    /// `world` | `model` | `observation` | `experience` (free-form on disk).
    pub kind: String,
    pub title: String,
    /// "read me when …" routing line, the reason the index is cheap.
    pub hook: Option<String>,
    pub status: Option<String>,
    pub updated: Option<String>,
    pub path: PathBuf,
    /// Evidence entries declared in the frontmatter. A count, not the evidence:
    /// the index routes and must not restate it.
    pub evidence: usize,
    /// Host-owned sampling weight; `None` until the host records a read.
    pub weight: Option<f64>,
    /// Body length in characters, the fixed floor of the removal rule.
    pub body_chars: usize,
}

impl MemoryPage {
    /// Whether the page may appear in the index at all.
    fn is_listable(&self) -> bool {
        !matches!(self.status.as_deref(), Some("archive" | "invalidated"))
    }

    /// Disputed pages still list, but apart from the type sections: a
    /// contradicted conclusion must not read as settled knowledge.
    fn is_disputed(&self) -> bool {
        matches!(self.status.as_deref(), Some("disputed"))
    }
}

#[must_use]
pub fn memory_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(MEMORY_DIR_NAME)
}

#[must_use]
pub fn pages_dir(agent_dir: &Path) -> PathBuf {
    memory_dir(agent_dir).join(PAGES_DIR_NAME)
}

/// Read every page under `root/pages`, skipping unreadable files (a corrupt page
/// must not blank the index of a session that is otherwise fine).
#[must_use]
pub fn load_pages(agent_dir: &Path) -> Vec<MemoryPage> {
    let root = pages_dir(agent_dir);
    let mut pages = Vec::new();
    collect(&root, &mut pages);
    pages.sort_by(|a, b| {
        (a.kind.as_str(), a.slug.as_str()).cmp(&(b.kind.as_str(), b.slug.as_str()))
    });
    pages
}

fn collect(dir: &Path, out: &mut Vec<MemoryPage>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `_entities` holds structure nodes: real pages, but not knowledge
            // units, so they never reach the index.
            if path.file_name().is_some_and(|name| name == "_entities") {
                continue;
            }
            collect(&path, out);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "md") {
            if let Some(page) = parse_page(&path) {
                if page.is_listable() {
                    out.push(page);
                }
            }
        }
    }
}

fn parse_page(path: &Path) -> Option<MemoryPage> {
    let text = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&text)?;
    let fields: serde_yaml::Value = serde_yaml::from_str(frontmatter).unwrap_or_default();
    let field = |key: &str| {
        fields
            .get(key)
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_string)
    };
    let slug = field("slug").or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    })?;
    let title = field("title")?;
    let evidence = fields
        .get("evidence")
        .and_then(serde_yaml::Value::as_sequence)
        .map_or(0, Vec::len);
    Some(MemoryPage {
        slug,
        kind: field("type").unwrap_or_else(|| "other".to_string()),
        title,
        hook: field("hook"),
        status: field("status"),
        updated: field("updated").or_else(|| field("checked")),
        path: path.to_path_buf(),
        evidence,
        weight: fields.get("weight").and_then(serde_yaml::Value::as_f64),
        body_chars: body.trim().chars().count(),
    })
}

/// The frontmatter block and the body after it. A page without frontmatter is
/// not a page: the type and hook the index needs live there.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest.trim_start_matches(['\r', '\n']);
    let close = rest.find("\n---")?;
    let frontmatter = &rest[..close];
    let body = rest[close..].trim_start_matches(['-', '\r', '\n']);
    Some((frontmatter, body))
}

/// One page write: the host half of the page store (the kernel writes through
/// `rlm.harness`, the refine planner writes through here).
#[derive(Debug, Clone)]
pub struct PageWrite<'a> {
    pub agent_dir: &'a Path,
    pub slug: &'a str,
    /// One of [`MEMORY_TYPES`].
    pub kind: &'a str,
    pub title: &'a str,
    pub hook: Option<&'a str>,
    pub status: &'a str,
    pub body: &'a str,
    /// ISO date the write happened; the index shows `updated` as freshness.
    pub updated: &'a str,
}

/// Where a page with this slug already lives, whatever its type directory.
#[must_use]
pub fn find_page(agent_dir: &Path, slug: &str) -> Option<PathBuf> {
    let root = pages_dir(agent_dir);
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "_entities") {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "md") {
                let matches = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| split_frontmatter(&text).map(|(fm, _)| fm.to_string()))
                    .and_then(|fm| {
                        serde_yaml::from_str::<serde_yaml::Value>(&fm)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("slug")
                                    .and_then(serde_yaml::Value::as_str)
                                    .map(str::to_string)
                            })
                    });
                if matches.as_deref() == Some(slug) {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Write a page, keeping any frontmatter field the caller does not set.
///
/// # Errors
///
/// Propagates the directory and file failures; an unknown type is an error too.
pub fn write_page(write: &PageWrite<'_>) -> std::io::Result<PathBuf> {
    if !MEMORY_TYPES.contains(&write.kind) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown memory type {:?}", write.kind),
        ));
    }
    let existing = find_page(write.agent_dir, write.slug);
    let block = existing
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| split_frontmatter(&text).map(|(fm, _)| fm.to_string()))
        .map(|fm| fm.lines().map(str::to_string).collect::<Vec<String>>())
        .unwrap_or_default();
    let mut updates: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    updates.insert("slug", yaml_scalar(write.slug));
    updates.insert("type", write.kind.to_string());
    updates.insert("title", yaml_scalar(write.title));
    updates.insert("status", write.status.to_string());
    updates.insert("updated", write.updated.to_string());
    if let Some(hook) = write.hook {
        updates.insert("hook", yaml_scalar(hook));
    }
    let lines = merge_frontmatter(&block, &updates);
    let mut text = String::from("---\n");
    text.push_str(&lines.join("\n"));
    text.push_str("\n---\n\n");
    text.push_str(write.body.trim());
    text.push('\n');
    let target = pages_dir(write.agent_dir)
        .join(write.kind)
        .join(format!("{}.md", write.slug));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, text)?;
    if let Some(previous) = existing {
        if previous != target {
            let _ = std::fs::remove_file(previous);
        }
    }
    Ok(target)
}

/// Archive a page instead of unlinking it and leave a tombstone line, so a
/// mistaken removal is reversible and the next pass does not recreate it.
///
/// # Errors
///
/// Propagates the move and append failures.
pub fn archive_page(agent_dir: &Path, slug: &str, today: &str) -> std::io::Result<bool> {
    use std::fmt::Write as _;

    let Some(path) = find_page(agent_dir, slug) else {
        return Ok(false);
    };
    let title = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| split_frontmatter(&text).map(|(fm, _)| fm.to_string()))
        .and_then(|fm| {
            serde_yaml::from_str::<serde_yaml::Value>(&fm)
                .ok()
                .and_then(|value| {
                    value
                        .get("title")
                        .and_then(serde_yaml::Value::as_str)
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| slug.to_string());
    let archive = memory_dir(agent_dir).join("archive");
    std::fs::create_dir_all(&archive)?;
    let name = path.file_name().map_or_else(
        || format!("{slug}.md"),
        |name| name.to_string_lossy().into_owned(),
    );
    std::fs::rename(&path, archive.join(name))?;
    let log = memory_dir(agent_dir).join("_archive.md");
    let mut existing = std::fs::read_to_string(&log).unwrap_or_default();
    if existing.is_empty() {
        existing.push_str("# Archive\n\n");
    }
    let _ = writeln!(existing, "- {slug} — {title} (archived {today})");
    std::fs::write(&log, existing)?;
    Ok(true)
}

/// Rewrite the given top-level keys and keep every other line verbatim: nested
/// blocks (`evidence:`, `relations:`, `history:`) belong to their parent key and
/// survive an edit that does not touch it.
fn merge_frontmatter(
    block: &[String],
    updates: &std::collections::BTreeMap<&str, String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut written: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut skip_children = false;
    for line in block {
        let nested = line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-');
        if nested {
            if !skip_children {
                out.push(line.clone());
            }
            continue;
        }
        let Some((key, _)) = line.split_once(':') else {
            out.push(line.clone());
            continue;
        };
        let key = key.trim();
        if let Some(value) = updates.get(key) {
            skip_children = true;
            out.push(format!("{key}: {value}"));
            written.insert(key);
            continue;
        }
        skip_children = false;
        out.push(line.clone());
    }
    for (key, value) in updates {
        if !written.contains(*key) {
            out.push(format!("{key}: {value}"));
        }
    }
    out
}

fn yaml_scalar(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A render of the pages as the model-facing index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndex {
    pub text: String,
    /// Which ladder rung the render settled on (0 = full detail). Reported as a
    /// count by the adoption telemetry; never affects the text.
    pub rung: u8,
    /// Stable hash of the index material: the digest fingerprint reads it so a
    /// page edit re-delivers instead of freezing a stale index.
    pub fingerprint: String,
}

/// One rung of the truncation ladder: how much of a hook to keep, and how many
/// pages each type may list.
#[derive(Debug, Clone, Copy)]
struct Rung {
    hook_cap: usize,
    per_kind: usize,
}

/// Degrade in the order that costs a reader the least. Measured on a 12-page
/// Chinese corpus: full hooks 644 tok, no hooks 383, four pages per type 279, so
/// the hooks are the expensive part and are dropped before any page is hidden.
const LADDER: [Rung; 4] = [
    Rung {
        hook_cap: usize::MAX,
        per_kind: usize::MAX,
    },
    Rung {
        hook_cap: 40,
        per_kind: usize::MAX,
    },
    Rung {
        hook_cap: 0,
        per_kind: usize::MAX,
    },
    Rung {
        hook_cap: 0,
        per_kind: 4,
    },
];

/// Render the index, walking [`LADDER`] until the render fits `budget_chars`.
/// The last rung is returned even when it still does not fit: a silently empty
/// index would be worse than a long one, and the caller can see the size.
#[must_use]
pub fn render_index(pages: &[MemoryPage], budget_chars: usize) -> MemoryIndex {
    let fingerprint = fingerprint(pages);
    let mut text = String::new();
    let mut settled = 0_u8;
    for (index, rung) in LADDER.into_iter().enumerate() {
        text = render_with(pages, rung);
        settled = u8::try_from(index).unwrap_or(u8::MAX);
        if text.chars().count() <= budget_chars {
            break;
        }
    }
    MemoryIndex {
        text,
        rung: settled,
        fingerprint,
    }
}

fn render_with(pages: &[MemoryPage], rung: Rung) -> String {
    use std::fmt::Write as _;

    let total = pages.len();
    let mut out = String::new();
    out.push_str("# Memory\n\n");
    let counts = MEMORY_TYPES
        .iter()
        .map(|kind| format!("{kind} {}", count_of(pages, kind)))
        .collect::<Vec<_>>()
        .join(" / ");
    let _ = writeln!(out, "Pages: {total} ({counts})\n");
    out.push_str(
        "Each line routes: read the page file for its body. `read when` is the situation that makes it relevant.\n\n",
    );
    for kind in MEMORY_TYPES {
        let section: Vec<&MemoryPage> = pages
            .iter()
            .filter(|page| page.kind == kind && !page.is_disputed())
            .collect();
        if section.is_empty() {
            continue;
        }
        let shown = listed(&section, rung.per_kind);
        let _ = writeln!(out, "## {kind} ({})", section.len());
        for page in &shown {
            let _ = write!(out, "- {} — {}", page.slug, page.title);
            if rung.hook_cap > 0 {
                if let Some(hook) = &page.hook {
                    let _ = write!(out, "\n  · read when: {}", truncate(hook, rung.hook_cap));
                }
            }
            out.push('\n');
        }
        let hidden = section.len() - shown.len();
        if hidden > 0 {
            let _ = writeln!(out, "- +{hidden} more {kind}");
        }
        out.push('\n');
    }
    let disputed: Vec<&MemoryPage> = pages.iter().filter(|page| page.is_disputed()).collect();
    if !disputed.is_empty() {
        let _ = writeln!(out, "## disputed ({})", disputed.len());
        for page in disputed {
            let _ = writeln!(
                out,
                "- {} — {} (contradicted; read before relying)",
                page.slug, page.title
            );
        }
        out.push('\n');
    }
    out
}

/// The pages of one type that make the cut, newest first: `updated` is the only
/// per-page freshness signal the index has, so it decides what a tight budget
/// hides.
fn listed<'a>(section: &[&'a MemoryPage], per_kind: usize) -> Vec<&'a MemoryPage> {
    if per_kind == usize::MAX || section.len() <= per_kind {
        return section.to_vec();
    }
    let mut sorted = section.to_vec();
    sorted.sort_by(|a, b| {
        b.updated
            .as_deref()
            .unwrap_or("")
            .cmp(a.updated.as_deref().unwrap_or(""))
    });
    sorted.truncate(per_kind);
    sorted
}

fn count_of(pages: &[MemoryPage], kind: &str) -> usize {
    pages.iter().filter(|page| page.kind == kind).count()
}

fn truncate(text: &str, cap: usize) -> String {
    if cap == usize::MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

fn fingerprint(pages: &[MemoryPage]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"memory-index-v1\0");
    for page in pages {
        hasher.update(page.kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(page.slug.as_bytes());
        hasher.update(b"\0");
        hasher.update(page.title.as_bytes());
        hasher.update(b"\0");
        hasher.update(page.hook.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(slug: &str, kind: &str, title: &str, hook: Option<&str>) -> MemoryPage {
        MemoryPage {
            slug: slug.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            hook: hook.map(str::to_string),
            status: None,
            updated: None,
            path: PathBuf::from(format!("/pages/{kind}/{slug}.md")),
            evidence: 0,
            weight: None,
            body_chars: 0,
        }
    }

    #[test]
    fn index_groups_pages_by_type_and_keeps_hooks() {
        let pages = vec![
            page("w1", "world", "globset star", Some("touching globs")),
            page("m1", "model", "forks own the tracker", Some("opening a PR")),
        ];
        let index = render_index(&pages, 10_000);
        assert!(index.text.contains("world 1 / model 1"));
        assert!(index.text.contains("## world (1)"));
        assert!(index.text.contains("· read when: touching globs"));
    }

    #[test]
    fn index_drops_hooks_before_hiding_any_page() {
        // Second rung: routing lines go, slugs stay. The budget is derived from
        // the full render, so the ladder must engage regardless of prose length.
        let pages = vec![
            page("w1", "world", "a fact", Some("always")),
            page("m1", "model", "a rule", Some("when editing")),
            page("o1", "observation", "a pattern", Some("when measuring")),
        ];
        let full = render_index(&pages, 10_000);
        let tight = render_index(&pages, full.text.chars().count() - 30);
        // The header always mentions `read when`; the bullet is what the rung drops.
        assert!(full.text.contains("· read when:"));
        assert!(!tight.text.contains("· read when:"), "{}", tight.text);
        for slug in ["w1", "m1", "o1"] {
            assert!(tight.text.contains(slug), "{slug} vanished: {}", tight.text);
        }
    }

    #[test]
    fn index_hides_the_stalest_pages_only_at_the_last_rung() {
        let mut pages: Vec<MemoryPage> = (0..6)
            .map(|i| {
                page(
                    &format!("o{i}"),
                    "observation",
                    "a pattern",
                    Some("when measuring"),
                )
            })
            .collect();
        for (i, page) in pages.iter_mut().enumerate() {
            page.updated = Some(format!("2026-11-{:02}", i + 1));
        }
        let tiny = render_index(&pages, 120);
        assert!(tiny.text.contains("+2 more observation"), "{}", tiny.text);
        // Freshest four survive, oldest two are the ones withheld.
        assert!(tiny.text.contains("o5"));
        assert!(!tiny.text.contains("- o0 "));
    }

    #[test]
    fn page_schema_reads_evidence_weight_and_body_length() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let model_dir = pages_dir(agent).join("observation");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            model_dir.join("ev.md"),
            "---\nslug: ev\ntype: observation\ntitle: Ev\nhook: when testing\nstatus: active\nweight: 0.75\nevidence:\n  - kind: command\n    ref: \"cargo test (2026-11-09)\"\n  - kind: session\n    ref: \"01a1e2fb\"\n---\n\nfour candles here\n",
        )
        .unwrap();
        let pages = load_pages(agent);
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.evidence, 2);
        assert_eq!(page.weight, Some(0.75));
        assert_eq!(page.body_chars, "four candles here".chars().count());
        assert_eq!(page.hook.as_deref(), Some("when testing"));
    }

    #[test]
    fn disputed_pages_list_apart_from_their_type() {
        let mut disputed = page("d1", "observation", "contradicted claim", None);
        disputed.status = Some("disputed".to_string());
        let pages = vec![page("o1", "observation", "settled claim", None), disputed];
        let index = render_index(&pages, 10_000);
        assert!(index.text.contains("## observation (1)"), "{}", index.text);
        assert!(index.text.contains("## disputed (1)"), "{}", index.text);
        assert!(index.text.contains("contradicted; read before relying"));
    }

    #[test]
    fn write_page_keeps_fields_the_caller_omits() {
        fn write<'a>(agent: &'a Path, hook: Option<&'a str>, body: &'a str) -> PageWrite<'a> {
            PageWrite {
                agent_dir: agent,
                slug: "keep",
                kind: "observation",
                title: "Keep",
                hook,
                status: "active",
                body,
                updated: "2026-11-17",
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let path = write_page(&write(agent, Some("when keeping"), "first body")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hook: \"when keeping\""));
        // A field the writer never sets (evidence) survives a later write.
        let with_evidence = text.replace(
            "status: active",
            "status: active\nevidence:\n  - kind: command\n    ref: \"cargo test\"",
        );
        std::fs::write(&path, with_evidence).unwrap();
        let path = write_page(&write(agent, None, "second body")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind: command"), "{text}");
        assert!(text.contains("hook: \"when keeping\""), "{text}");
        assert!(text.contains("second body"), "{text}");
        assert_eq!(text.matches("slug:").count(), 1, "{text}");
    }

    #[test]
    fn archive_page_moves_the_file_and_leaves_a_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        write_page(&PageWrite {
            agent_dir: agent,
            slug: "gone",
            kind: "model",
            title: "Gone",
            hook: None,
            status: "active",
            body: "body",
            updated: "2026-11-17",
        })
        .unwrap();
        assert!(archive_page(agent, "gone", "2026-11-17").unwrap());
        assert!(load_pages(agent).is_empty());
        let log = std::fs::read_to_string(memory_dir(agent).join("_archive.md")).unwrap();
        assert!(log.contains("gone — Gone (archived 2026-11-17)"), "{log}");
        assert!(!archive_page(agent, "gone", "2026-11-17").unwrap());
    }

    #[test]
    fn load_pages_reads_frontmatter_and_skips_entities() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let model_dir = pages_dir(agent).join("model");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::create_dir_all(pages_dir(agent).join("_entities")).unwrap();
        std::fs::write(
            model_dir.join("one.md"),
            "---\nslug: one\ntype: model\ntitle: One\nhook: when one\nstatus: active\nrelations:\n  - {type: part_of, target: p}\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(
            pages_dir(agent).join("_entities").join("p.md"),
            "---\nslug: p\ntype: entity\ntitle: Project\n---\n",
        )
        .unwrap();
        let pages = load_pages(agent);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "one");
        assert_eq!(pages[0].hook.as_deref(), Some("when one"));
    }

    #[test]
    fn archived_pages_never_reach_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let dir_path = pages_dir(agent).join("observation");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(
            dir_path.join("old.md"),
            "---\nslug: old\ntype: observation\ntitle: Old\nstatus: archive\n---\n",
        )
        .unwrap();
        assert!(load_pages(agent).is_empty());
    }
}
