//! Page-backed memory: pages are the source of truth, the resident index is a
//! render of them.
//!
//! One directory of markdown files per agent (`<agent dir>/memory/pages/<type>/`)
//! plus a generated index (`MEMORY.md`). The digest renders the index; the model
//! reads a page body with its normal file tools, so the index only has to make a
//! page findable, not restate it.

use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl MemoryPage {
    /// Whether the page may appear in the index at all.
    fn is_listable(&self) -> bool {
        !matches!(self.status.as_deref(), Some("archive" | "invalidated"))
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
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut fields: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        // Nested blocks (`relations:`, `evidence:`, `history:`) are not index
        // material and would otherwise overwrite their parent key with junk.
        if value.trim().is_empty() && key.trim().chars().all(|c| c == ' ' || c.is_alphabetic()) {
            continue;
        }
        fields.insert(key.trim().to_string(), unquote(value.trim()));
    }
    let slug = fields.get("slug").cloned().or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    })?;
    let title = fields.get("title").cloned()?;
    Some(MemoryPage {
        slug,
        kind: fields
            .get("type")
            .cloned()
            .unwrap_or_else(|| "other".to_string()),
        title,
        hook: fields.get("hook").cloned(),
        status: fields.get("status").cloned(),
        updated: fields
            .get("updated")
            .or_else(|| fields.get("checked"))
            .cloned(),
        path: path.to_path_buf(),
    })
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
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
        let section: Vec<&MemoryPage> = pages.iter().filter(|page| page.kind == kind).collect();
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
