//! The dream pass: when memory consolidation runs, what it is told, and how
//! its run is recorded.
//!
//! The pass is a subagent with a dedicated prompt, not a public entrypoint: it
//! reads the page store, decides where a walk starts and stops, and records what
//! it did. This module owns the parts that must be deterministic — due-ness, the
//! next run time, the node budget, the lock, and the journal — so the judgement
//! stays in the prompt while the bookkeeping stays auditable.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{load_pages, memory_dir};

/// Floor between passes: a pass that wakes more often than this is churn.
pub const DREAM_MIN_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Ceiling between passes: a pass that keeps postponing itself must still run.
pub const DREAM_MAX_INTERVAL_SECS: u64 = 14 * 24 * 60 * 60;
/// Nodes one pass may look at before it must stop.
pub const DREAM_NODE_BUDGET: usize = 24;
/// Below this body length a page carries no conclusion worth keeping.
pub const REMOVAL_BODY_FLOOR: usize = 120;
/// A lock older than this is treated as abandoned (a killed pass must not wedge
/// the store forever).
pub const DREAM_LOCK_STALE_SECS: u64 = 30 * 60;

/// One journal line: what a pass did, and when it wants to run again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DreamRecord {
    /// Epoch seconds the pass started.
    pub ts: u64,
    /// Which seed rule picked the nodes (`draft`, `stale`, `unverified`).
    pub seed: String,
    pub visited: usize,
    pub compressed: usize,
    pub merged: usize,
    pub promoted: usize,
    pub removed: usize,
    /// Epoch seconds the pass wants to run again.
    pub next_at: u64,
    /// Why it chose that time, in words a human can read.
    pub why: String,
}

/// Whether a pass is due, and the nodes it should start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamDecision {
    pub due: bool,
    pub reason: String,
    /// Page slugs the walk may start at, freshest judgement first.
    pub seeds: Vec<String>,
}

/// Where the pass writes its journal.
#[must_use]
pub fn journal_path(agent_dir: &Path) -> PathBuf {
    memory_dir(agent_dir)
        .join("journal")
        .join("dream.log.jsonl")
}

/// The recorded passes, oldest first. A corrupt line is skipped: a bad journal
/// line must not stop consolidation forever.
#[must_use]
pub fn read_journal(agent_dir: &Path) -> Vec<DreamRecord> {
    let Ok(text) = std::fs::read_to_string(journal_path(agent_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<DreamRecord>(line.trim()).ok())
        .collect()
}

/// Append one pass record.
///
/// # Errors
///
/// Propagates the directory and append failures.
pub fn append_journal(agent_dir: &Path, record: &DreamRecord) -> std::io::Result<()> {
    let path = journal_path(agent_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    writeln!(file, "{line}")
}

/// Clamp a requested next run time into the allowed window: the pass may choose
/// its own rhythm, not an unbounded one.
#[must_use]
pub fn clamp_next_at(now: u64, requested: Option<u64>) -> u64 {
    let earliest = now + DREAM_MIN_INTERVAL_SECS;
    let latest = now + DREAM_MAX_INTERVAL_SECS;
    requested.unwrap_or(latest).clamp(earliest, latest)
}

/// Should a pass run now? Answered from the journal and the page store only, so
/// the same inputs always give the same answer.
#[must_use]
pub fn dream_due(agent_dir: &Path, now: u64) -> DreamDecision {
    let pages = load_pages(agent_dir);
    if pages.is_empty() {
        return DreamDecision {
            due: false,
            reason: "no memory pages".to_string(),
            seeds: Vec::new(),
        };
    }
    let journal = read_journal(agent_dir);
    if let Some(last) = journal.last() {
        if now < last.next_at {
            return DreamDecision {
                due: false,
                reason: format!("next pass at {}", last.next_at),
                seeds: Vec::new(),
            };
        }
    }
    DreamDecision {
        due: true,
        reason: if journal.is_empty() {
            "no pass recorded yet".to_string()
        } else {
            "next pass is due".to_string()
        },
        seeds: seeds(&pages),
    }
}

/// Walk seeds, ordered by how much they need looking at: drafts first (not yet
/// settled), then the least recently touched, then pages with no evidence.
fn seeds(pages: &[super::MemoryPage]) -> Vec<String> {
    let mut ordered: Vec<&super::MemoryPage> = pages.iter().collect();
    ordered.sort_by_key(|page| {
        let draft = !matches!(page.status.as_deref(), Some("active"));
        let unverified = page.evidence == 0;
        let updated = page.updated.clone().unwrap_or_default();
        (!draft, !unverified, updated)
    });
    ordered
        .into_iter()
        .take(DREAM_NODE_BUDGET)
        .map(|page| page.slug.clone())
        .collect()
}

/// Take the store for one pass. `false` means another pass holds it.
///
/// # Errors
///
/// Propagates the directory and file failures.
pub fn acquire_lock(agent_dir: &Path, now: u64) -> std::io::Result<bool> {
    let path = memory_dir(agent_dir).join(".dream.lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map(|mut file| {
                let _ = writeln!(file, "{now}");
            })
            .is_ok());
    };
    let held_since = contents.trim().parse::<u64>().unwrap_or(0);
    if now.saturating_sub(held_since) < DREAM_LOCK_STALE_SECS {
        return Ok(false);
    }
    std::fs::write(&path, format!("{now}\n"))?;
    Ok(true)
}

/// Release the pass lock.
pub fn release_lock(agent_dir: &Path) {
    let _ = std::fs::remove_file(memory_dir(agent_dir).join(".dream.lock"));
}

/// The heartbeat interval string for the pass's chosen next run time. The
/// scheduler primitive in this runtime is a recurring, session-bound heartbeat,
/// so a pass schedules its successor with `rlm_heartbeat.create(...)`.
#[must_use]
pub fn heartbeat_interval(next_at: u64, now: u64) -> String {
    let seconds = clamp_next_at(now, Some(next_at)).saturating_sub(now);
    let hours = seconds.div_ceil(3_600).max(1);
    format!("every {hours}h")
}

/// A deterministic action a pass may take without judgement: the rules decide,
/// the model only has to agree it looked at the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DreamAction {
    /// `experience` with at least two independent evidence entries.
    Promote { slug: String, evidence: usize },
    /// Eligible for removal: short body, no evidence, nothing links to it.
    Remove { slug: String, body_chars: usize },
}

/// The actions the rules already justify, given the page store as it is now.
///
/// Distillation, relating, abstracting, and merging stay out: they need the
/// pass's judgement. This list is only what the preconditions decide, so a pass
/// cannot remove a page the rules would keep.
#[must_use]
pub fn plan_pass(agent_dir: &Path) -> Vec<DreamAction> {
    let pages = load_pages(agent_dir);
    let linked = connected_slugs(agent_dir);
    let mut actions = Vec::new();
    for page in &pages {
        if page.is_protected() {
            continue;
        }
        if page.kind == "experience" && page.evidence >= 2 {
            actions.push(DreamAction::Promote {
                slug: page.slug.clone(),
                evidence: page.evidence,
            });
            continue;
        }
        let removable = page.body_chars < REMOVAL_BODY_FLOOR
            && page.evidence == 0
            && !linked.contains(&page.slug)
            && !page.is_disputed();
        if removable {
            actions.push(DreamAction::Remove {
                slug: page.slug.clone(),
                body_chars: page.body_chars,
            });
        }
    }
    actions
}

/// Promotion: an `experience` page becomes `observation` once two independent
/// pieces of evidence support it.
///
/// # Errors
///
/// Propagates the read and write failures.
pub fn promote_page(agent_dir: &Path, slug: &str, today: &str) -> std::io::Result<bool> {
    let Some(page) = load_pages(agent_dir)
        .into_iter()
        .find(|page| page.slug == slug)
    else {
        return Ok(false);
    };
    if page.kind != "experience" || page.evidence < 2 {
        return Ok(false);
    }
    let body = std::fs::read_to_string(&page.path)
        .ok()
        .and_then(|text| super::split_frontmatter(&text).map(|(_, body)| body.to_string()))
        .unwrap_or_default();
    super::write_page(&super::PageWrite {
        agent_dir,
        slug: &page.slug,
        kind: "observation",
        title: &page.title,
        hook: page.hook.as_deref(),
        status: "active",
        body: &body,
        updated: today,
    })?;
    Ok(true)
}

/// Removal, decided by the rules alone: short body, no evidence, no inbound
/// link. The page is archived, never unlinked.
///
/// # Errors
///
/// Propagates the read and archive failures.
pub fn remove_page(agent_dir: &Path, slug: &str, today: &str) -> std::io::Result<bool> {
    let pages = load_pages(agent_dir);
    let Some(page) = pages.iter().find(|page| page.slug == slug) else {
        return Ok(false);
    };
    let linked = connected_slugs(agent_dir);
    let eligible = !page.is_protected()
        && page.body_chars < REMOVAL_BODY_FLOOR
        && page.evidence == 0
        && !linked.contains(&page.slug)
        && !page.is_disputed();
    if !eligible {
        return Ok(false);
    }
    super::archive_page(agent_dir, slug, today)
}

/// Slugs that take part in any edge: a page something points at, and a page that
/// points somewhere. Either end is treated as structural, because removing it
/// would leave the other end dangling.
fn connected_slugs(agent_dir: &Path) -> std::collections::BTreeSet<String> {
    let mut targets = std::collections::BTreeSet::new();
    let root = super::pages_dir(agent_dir);
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut has_edge = false;
            for line in text.lines() {
                let trimmed = line.trim_start();
                if let Some(rest) = trimmed.strip_prefix("target:") {
                    targets.insert(rest.trim().trim_matches('"').trim_matches('\'').to_string());
                    has_edge = true;
                } else if let Some(rest) = trimmed.strip_prefix("relations:") {
                    has_edge = !rest.trim().is_empty();
                }
            }
            if has_edge {
                if let Some((frontmatter, _)) = super::split_frontmatter(&text) {
                    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(frontmatter) {
                        if let Some(slug) = value.get("slug").and_then(serde_yaml::Value::as_str) {
                            targets.insert(slug.to_string());
                        }
                    }
                }
            }
        }
    }
    targets
}

/// The instruction block the pass is spawned with. Scope, budget, and the
/// write discipline are stated here because the pass decides its own walk.
#[must_use]
pub fn dream_prompt(decision: &DreamDecision) -> String {
    let seeds = if decision.seeds.is_empty() {
        "(none: the store is empty)".to_string()
    } else {
        decision.seeds.join(", ")
    };
    format!(
        "You are the memory consolidation pass for this agent's page store at \
`<agent dir>/memory/pages/`. You are not a chat assistant here: you maintain pages, and you \
decide where the walk starts and stops.\n\n\
Start from these seeds (most in need of attention first): {seeds}.\n\n\
What you may do, in this order of preference:\n\
1. Relate — add `part_of` / `derived_from` / `contradicts` edges between pages that already exist. Edges only point at existing pages.\n\
2. Distill — shorten a page to its conclusion, its trigger, and one evidence pointer. Extraction only: delete sentences, never invent a conclusion.\n\
3. Abstract — from two or more pages with the same conclusion, write one `model` page and link the sources.\n\
4. Promote — an `experience` page becomes `observation` only when at least two independent pieces of evidence support it.\n\
5. Merge — two pages with the same conclusion become one; keep the surviving slug and leave a tombstone for the other.\n\
6. Remove — only when ALL hold: the page body is under 120 characters, you searched this pass for evidence supporting its conclusion and found none, nothing links to it, and it is not an `_entities` node. Archive it; never unlink.\n\n\
Evidence is what makes a conclusion trustworthy: cite what you relied on as a resolvable reference \
(command and date, session id and turn, file and line, or commit sha). Evidence written in this \
same pass never justifies a removal.\n\
Stop when you have visited {DREAM_NODE_BUDGET} pages, after three rounds, or when the remaining work needs a \
decision you cannot support with evidence. Then choose when this pass should run again \
(between six hours and fourteen days), say why in one sentence, and record the pass.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(slug: &str, status: &str, evidence: usize) -> super::super::MemoryPage {
        super::super::MemoryPage {
            slug: slug.to_string(),
            kind: "observation".to_string(),
            title: slug.to_string(),
            hook: None,
            status: Some(status.to_string()),
            updated: Some("2026-11-01".to_string()),
            path: PathBuf::from("x.md"),
            evidence,
            weight: None,
            body_chars: 200,
        }
    }

    #[test]
    fn clamp_keeps_the_rhythm_inside_the_window() {
        let now = 1_000_000;
        assert_eq!(clamp_next_at(now, None), now + DREAM_MAX_INTERVAL_SECS);
        assert_eq!(clamp_next_at(now, Some(now)), now + DREAM_MIN_INTERVAL_SECS);
        assert_eq!(
            clamp_next_at(now, Some(now + 10 * DREAM_MAX_INTERVAL_SECS)),
            now + DREAM_MAX_INTERVAL_SECS
        );
    }

    #[test]
    fn dream_is_due_until_a_pass_records_a_next_time() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dream_due(dir.path(), 10).due);
        let target = super::super::pages_dir(dir.path()).join("observation");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            target.join("a.md"),
            "---\nslug: a\ntype: observation\ntitle: A\nstatus: draft\n---\n\nbody\n",
        )
        .unwrap();
        assert!(dream_due(dir.path(), 1_000).due);
        append_journal(
            dir.path(),
            &DreamRecord {
                ts: 1_000,
                seed: "draft".to_string(),
                visited: 1,
                compressed: 0,
                merged: 0,
                promoted: 0,
                removed: 0,
                next_at: 5_000,
                why: "nothing else due".to_string(),
            },
        )
        .unwrap();
        assert!(!dream_due(dir.path(), 4_000).due);
        assert!(dream_due(dir.path(), 5_000).due);
        assert_eq!(read_journal(dir.path()).len(), 1);
    }

    #[test]
    fn seeds_put_drafts_first_and_respect_the_budget() {
        let pages = vec![
            page("settled", "active", 2),
            page("draft", "draft", 0),
            page("bare", "active", 0),
        ];
        assert_eq!(seeds(&pages), vec!["draft", "bare", "settled"]);
    }

    #[test]
    fn the_lock_admits_one_pass_and_expires_when_stale() {
        let dir = tempfile::tempdir().unwrap();
        assert!(acquire_lock(dir.path(), 1_000).unwrap());
        assert!(!acquire_lock(dir.path(), 1_100).unwrap());
        assert!(acquire_lock(dir.path(), 1_000 + DREAM_LOCK_STALE_SECS + 1).unwrap());
        release_lock(dir.path());
        assert!(acquire_lock(dir.path(), 2_000).unwrap());
    }

    #[test]
    fn the_next_run_becomes_a_heartbeat_interval() {
        let now = 1_000_000;
        assert_eq!(heartbeat_interval(now + 12 * 3_600, now), "every 12h");
        assert_eq!(heartbeat_interval(now, now), "every 6h");
        assert_eq!(heartbeat_interval(now + 40 * 24 * 3_600, now), "every 336h");
    }

    #[test]
    fn promotions_need_two_evidence_entries() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let write = |slug: &str, kind: &str, evidence: &str, body: &str| {
            let path = super::super::pages_dir(agent).join(kind);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join(format!("{slug}.md")),
                format!(
                    "---\nslug: {slug}\ntype: {kind}\ntitle: {slug}\nstatus: active\n{evidence}---\n\n{body}\n"
                ),
            )
            .unwrap();
        };
        write("thin", "experience", "", "one run only");
        write(
            "ready",
            "experience",
            "evidence:\n  - kind: command\n    ref: a\n  - kind: session\n    ref: b\n",
            "two runs agree",
        );
        let actions = plan_pass(agent);
        assert!(
            actions.contains(&DreamAction::Promote {
                slug: "ready".to_string(),
                evidence: 2
            }),
            "{actions:?}"
        );
        assert!(!actions.iter().any(|action| matches!(
            action,
            DreamAction::Promote { slug, .. } if slug == "thin"
        )));
        assert!(promote_page(agent, "ready", "2026-11-17").unwrap());
        assert!(!promote_page(agent, "thin", "2026-11-17").unwrap());
        let promoted = load_pages(agent);
        assert_eq!(
            promoted
                .iter()
                .find(|page| page.slug == "ready")
                .map(|page| page.kind.as_str()),
            Some("observation")
        );
    }

    #[test]
    fn removal_needs_a_short_body_no_evidence_and_no_inbound_link() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let write = |slug: &str, body: &str, extra: &str| {
            let path = super::super::pages_dir(agent).join("observation");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join(format!("{slug}.md")),
                format!("---\nslug: {slug}\ntype: observation\ntitle: {slug}\nstatus: active\n{extra}---\n\n{body}\n"),
            )
            .unwrap();
        };
        write("tiny", "short", "");
        write("long", &"x".repeat(200), "");
        write("cited", "short", "evidence:\n  - kind: quote\n    ref: q\n");
        write(
            "linked",
            "short",
            "relations:\n  - type: part_of\n    target: proj\n",
        );
        let actions = plan_pass(agent);
        assert!(
            actions.contains(&DreamAction::Remove {
                slug: "tiny".to_string(),
                body_chars: 5
            }),
            "{actions:?}"
        );
        for slug in ["long", "cited", "linked"] {
            assert!(
                !actions.iter().any(|action| matches!(
                    action,
                    DreamAction::Remove { slug: found, .. } if found == slug
                )),
                "{slug} must not be a removal candidate: {actions:?}"
            );
        }
        assert!(remove_page(agent, "tiny", "2026-11-17").unwrap());
        assert!(!remove_page(agent, "long", "2026-11-17").unwrap());
    }

    #[test]
    fn the_prompt_states_the_budget_and_the_seeds() {
        let decision = DreamDecision {
            due: true,
            reason: "x".to_string(),
            seeds: vec!["a".to_string(), "b".to_string()],
        };
        let prompt = dream_prompt(&decision);
        assert!(prompt.contains("a, b"));
        assert!(prompt.contains(&DREAM_NODE_BUDGET.to_string()));
        assert!(prompt.contains("never unlink"));
    }
}
