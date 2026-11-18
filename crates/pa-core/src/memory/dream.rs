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
        seeds: seeds(&pages, (now / 86_400) as i64),
    }
}

/// Walk seeds, ordered by how much they need looking at: drafts first (not yet
/// settled), then pages with no evidence, then the least-read (decayed weight).
#[must_use]
fn seeds(pages: &[super::MemoryPage], today: i64) -> Vec<String> {
    let mut ordered: Vec<&super::MemoryPage> = pages.iter().collect();
    ordered.sort_by(|left, right| {
        let draft = |page: &super::MemoryPage| !matches!(page.status.as_deref(), Some("active"));
        let unverified = |page: &super::MemoryPage| page.evidence == 0;
        draft(right)
            .cmp(&draft(left))
            .then(unverified(right).cmp(&unverified(left)))
            .then(effective_weight(left, today).total_cmp(&effective_weight(right, today)))
            .then(left.slug.cmp(&right.slug))
    });
    ordered
        .into_iter()
        .take(DREAM_NODE_BUDGET)
        .map(|page| page.slug.clone())
        .collect()
}

/// The frontmatter block as lines, ready to be edited and re-joined.
fn front_lines(text: &str) -> (Vec<String>, String) {
    match super::split_frontmatter(text) {
        Some((front, body)) => (
            front
                .lines()
                .map(|line| line.trim_end().to_string())
                .collect(),
            body.to_string(),
        ),
        None => (Vec::new(), text.to_string()),
    }
}

/// The list entries declared under `key`, so a merge can carry them over.
fn block_lines(front: &[String], key: &str) -> Vec<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut inside = false;
    for line in front {
        if line.starts_with(&format!("{key}:")) {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.starts_with("  - ") || line.starts_with("- ") {
            collected.push(line.clone());
        } else if line.starts_with("    ") {
            // A list entry is a mapping: its remaining fields belong to it, or a
            // merge would carry the key and drop the proof.
            if let Some(last) = collected.last_mut() {
                last.push('\n');
                last.push_str(line);
            }
        } else {
            inside = false;
        }
    }
    collected
}

/// Append one list entry to `key`, creating the key when the page has none.
fn insert_into_block(front: Vec<String>, key: &str, entry: &str) -> Vec<String> {
    // The entry keeps its own indentation: YAML needs one list to line up.
    let entry = entry.trim_end();
    let mut out: Vec<String> = Vec::with_capacity(front.len() + 2);
    let mut inserted = false;
    let mut inside = false;
    let mut has_key = false;
    for line in front {
        let is_entry_start = line.starts_with("  - ") || line.starts_with("- ");
        let is_continuation = line.starts_with("    ") || line.starts_with('\t');
        if inside && !is_entry_start && !is_continuation {
            // Left the block: this is where the new entry belongs.
            if !inserted {
                out.push(entry.to_string());
                inserted = true;
            }
            inside = false;
        }
        if line.starts_with(&format!("{key}:")) {
            inside = true;
            has_key = true;
        }
        out.push(line);
    }
    if !inserted {
        if has_key {
            out.push(entry.to_string());
        } else {
            out.push(format!("{key}:"));
            out.push(format!("  {}", entry.trim_start()));
        }
    }
    out
}

/// Merge `absorb` into `keep`: the pass decides that two pages say the same
/// thing, this does the writing. Evidence is carried over (so the survivor can
/// still be promoted on it), body lines survive once, every page that pointed at
/// the absorbed slug points at the survivor, and the absorbed page is archived
/// with a tombstone — a merge must never lose where a claim came from.
///
/// Returns false when either page is missing, when they are the same page, or
/// when the absorbed page is protected.
///
/// # Errors
///
/// Propagates read, write and archive failures.
pub fn merge_pages(
    agent_dir: &Path,
    keep: &str,
    absorb: &str,
    today: &str,
) -> std::io::Result<bool> {
    if keep == absorb {
        return Ok(false);
    }
    let pages = load_pages(agent_dir);
    let Some(survivor) = pages.iter().find(|page| page.slug == keep) else {
        return Ok(false);
    };
    let Some(absorbed) = pages.iter().find(|page| page.slug == absorb) else {
        return Ok(false);
    };
    if absorbed.is_protected() {
        return Ok(false);
    }

    let (mut front, survivor_body) = front_lines(&std::fs::read_to_string(&survivor.path)?);
    let (absorbed_front, absorbed_body) = front_lines(&std::fs::read_to_string(&absorbed.path)?);

    // Body: survivor first, then only the absorbed lines that are new.
    let mut lines: Vec<String> = survivor_body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim_end().to_string())
        .collect();
    for line in absorbed_body.lines() {
        let line = line.trim_end();
        if !line.trim().is_empty() && !lines.iter().any(|existing| existing == line) {
            lines.push(line.to_string());
        }
    }

    // Evidence: carry the absorbed entries so the survivor keeps its proof.
    for entry in block_lines(&absorbed_front, "evidence") {
        let already = block_lines(&front, "evidence")
            .iter()
            .any(|existing| existing.trim() == entry.trim());
        if !already {
            front = insert_into_block(front, "evidence", &entry);
        }
    }

    let body = lines.join("\n");
    std::fs::write(
        &survivor.path,
        format!("---\n{}\n---\n\n{body}\n", front.join("\n")),
    )?;
    super::archive_page(agent_dir, absorb, &format!("merged into {keep} ({today})"))?;

    // A dangling reference is worse than a stale one: point every page at the
    // survivor, not only the pages in the same type.
    for page in load_pages(agent_dir) {
        let Ok(text) = std::fs::read_to_string(&page.path) else {
            continue;
        };
        let (front, body) = front_lines(&text);
        let mut changed = false;
        let rewritten: Vec<String> = front
            .into_iter()
            .map(|line| {
                let trimmed = line.trim_start();
                if trimmed.starts_with("target:") && trimmed.ends_with(absorb) {
                    changed = true;
                    return line.replace(absorb, keep);
                }
                line
            })
            .collect();
        if changed {
            std::fs::write(
                &page.path,
                format!(
                    "---\n{}\n---\n\n{}\n",
                    rewritten.join("\n"),
                    body.trim_end()
                ),
            )?;
        }
    }
    Ok(true)
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
pub fn plan_pass(agent_dir: &Path, today: i64) -> Vec<DreamAction> {
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
            && !page.is_disputed()
            && is_aged(page, today);
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
    let today_day = iso_day(today).unwrap_or(i64::MAX);
    let pages = load_pages(agent_dir);
    let Some(page) = pages.iter().find(|page| page.slug == slug) else {
        return Ok(false);
    };
    let linked = connected_slugs(agent_dir);
    let eligible = !page.is_protected()
        && page.body_chars < REMOVAL_BODY_FLOOR
        && page.evidence == 0
        && !linked.contains(&page.slug)
        && !page.is_disputed()
        && is_aged(page, today_day);
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

/// Half-life of the read weight: a page nobody reads fades in about a month.
pub const WEIGHT_HALF_LIFE_DAYS: i64 = 30;
/// A page younger than this is never a removal candidate, however short it is: a
/// brand-new page has no weight yet, and "unread" must not read as "unwanted".
pub const REMOVAL_MIN_AGE_DAYS: i64 = 14;

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm), so
/// decay needs no date crate.
#[must_use]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The day number of an ISO `YYYY-MM-DD` prefix, if the page carries one.
#[must_use]
pub fn iso_day(text: &str) -> Option<i64> {
    let date = text.get(..10)?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    Some(days_from_civil(year, month, day))
}

/// The read weight after time decay: `weight * 0.5^(age / half_life)`.
/// An unread page decays from zero, so this never invents interest.
#[must_use]
pub fn effective_weight(page: &super::MemoryPage, today: i64) -> f64 {
    let weight = page.weight.unwrap_or(0.0);
    let Some(updated) = page.updated.as_deref().and_then(iso_day) else {
        return weight;
    };
    let age = (today - updated).max(0) as f64;
    weight * 2f64.powf(-age / WEIGHT_HALF_LIFE_DAYS as f64)
}

/// Whether a page is old enough to be a removal candidate at all.
#[must_use]
pub fn is_aged(page: &super::MemoryPage, today: i64) -> bool {
    page.updated
        .as_deref()
        .and_then(iso_day)
        .is_some_and(|updated| today - updated >= REMOVAL_MIN_AGE_DAYS)
}

/// Where the pass's instruction file lives while a pass is due. The digest points
/// the session at this file instead of restating the instruction every turn.
#[must_use]
pub fn prompt_path(agent_dir: &Path) -> PathBuf {
    memory_dir(agent_dir).join("DREAM.md")
}

/// Write the instruction file when a pass is due, so a session can hand it to a
/// subagent as its task. Returns the prompt when it wrote one.
///
/// # Errors
///
/// Propagates the directory and file failures.
pub fn ensure_prompt_file(
    agent_dir: &Path,
    decision: &DreamDecision,
) -> std::io::Result<Option<String>> {
    if !decision.due {
        return Ok(None);
    }
    let prompt = dream_prompt(decision);
    let path = prompt_path(agent_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &prompt)?;
    Ok(Some(prompt))
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
5. Merge — two pages with the same conclusion become one. Do it with `rlm.harness.merge_pages(keep, absorb)`, never by hand: the merge carries the evidence over and moves every reference to the survivor, and a hand edit loses both.\n\
6. Remove — only when ALL hold: the page body is under 120 characters, it was last written more than fourteen days ago, you searched this pass for evidence supporting its conclusion and found none, nothing links to it, and it is not an `_entities` node. Do it with `rlm.harness.delete_memory(slug)`: it archives the page and logs the reason. Never unlink a page.\n\n\
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

    /// 2026-11-17 as a day number: the fixtures were written around this date.
    const TODAY: i64 = 20_774;

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
        assert_eq!(seeds(&pages, 20_000), vec!["draft", "bare", "settled"]);
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
        let actions = plan_pass(agent, TODAY);
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

    fn write_page_at(agent: &std::path::Path, slug: &str, body: &str, extra: &str, updated: &str) {
        let path = super::super::pages_dir(agent).join("observation");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join(format!("{slug}.md")),
            format!(
                "---\nslug: {slug}\ntype: observation\ntitle: {slug}\nstatus: active\nupdated: {updated}\n{extra}---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn removal_needs_a_short_body_no_evidence_no_inbound_link_and_age() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let old = "2026-10-01";
        write_page_at(agent, "tiny", "short", "", old);
        write_page_at(agent, "long", &"x".repeat(200), "", old);
        write_page_at(
            agent,
            "cited",
            "short",
            "evidence:\n  - kind: quote\n    ref: q\n",
            old,
        );
        write_page_at(
            agent,
            "linked",
            "short",
            "relations:\n  - type: part_of\n    target: proj\n",
            old,
        );
        // Written today: unread is not unwanted, so it is not a candidate yet.
        write_page_at(agent, "fresh", "short", "", "2026-11-17");
        let actions = plan_pass(agent, TODAY);
        assert!(
            actions.contains(&DreamAction::Remove {
                slug: "tiny".to_string(),
                body_chars: 5
            }),
            "{actions:?}"
        );
        for slug in ["long", "cited", "linked", "fresh"] {
            assert!(
                !actions.iter().any(|action| matches!(
                    action,
                    DreamAction::Remove { slug: found, .. } if found == slug
                )),
                "{slug} must not be a removal candidate: {actions:?}"
            );
        }
        assert!(remove_page(agent, "tiny", "2026-11-17").unwrap());
        assert!(!remove_page(agent, "fresh", "2026-11-17").unwrap());
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
        // The prompt names the tools, so a pass edits through them and not by hand.
        assert!(prompt.contains("rlm.harness.merge_pages"));
        assert!(prompt.contains("rlm.harness.delete_memory"));
        assert!(prompt.contains("Never unlink a page"));
    }
}

#[cfg(test)]
mod decay_tests {
    use super::*;

    fn page(weight: f64, updated: &str) -> crate::memory::MemoryPage {
        crate::memory::MemoryPage {
            slug: "p".to_string(),
            kind: "observation".to_string(),
            title: "p".to_string(),
            hook: None,
            status: Some("active".to_string()),
            updated: Some(updated.to_string()),
            path: std::path::PathBuf::from("p.md"),
            evidence: 0,
            weight: Some(weight),
            body_chars: 10,
        }
    }

    #[test]
    fn weight_halves_in_a_month_and_never_goes_negative() {
        let today = iso_day("2026-11-17").unwrap();
        assert_eq!(effective_weight(&page(4.0, "2026-11-17"), today), 4.0);
        let month = effective_weight(&page(4.0, "2026-10-18"), today);
        assert!((month - 2.0).abs() < 0.05, "{month}");
        let two_months = effective_weight(&page(4.0, "2026-09-18"), today);
        assert!((two_months - 1.0).abs() < 0.08, "{two_months}");
        // A clock that jumps backwards must not inflate the weight.
        assert_eq!(effective_weight(&page(1.0, "2027-01-01"), today), 1.0);
        // An unread page stays unread, however old.
        assert_eq!(effective_weight(&page(0.0, "2020-01-01"), today), 0.0);
    }

    #[test]
    fn age_is_what_stops_a_fresh_page_from_being_removed() {
        let today = iso_day("2026-11-17").unwrap();
        assert!(!is_aged(&page(0.0, "2026-11-10"), today));
        assert!(is_aged(&page(0.0, "2026-11-03"), today));
        let mut undated = page(0.0, "2026-01-01");
        undated.updated = None;
        assert!(!is_aged(&undated, today), "no date means no deletion");
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn page(agent: &std::path::Path, slug: &str, body: &str, extra: &str) {
        let path = crate::memory::pages_dir(agent).join("observation");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join(format!("{slug}.md")),
            format!(
                "---\nslug: {slug}\ntype: observation\ntitle: {slug}\nstatus: active\nupdated: 2026-10-01\n{extra}---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_merge_keeps_one_body_one_proof_and_no_dangling_reference() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        page(
            agent,
            "keep",
            "the rule holds\nshared line",
            "evidence:\n  - kind: command\n    ref: cargo test\n",
        );
        page(
            agent,
            "absorb",
            "shared line\nthe rule also holds\n",
            "evidence:\n  - kind: issue\n    ref: '#92'\n",
        );
        page(
            agent,
            "points",
            "body",
            "relations:\n  - type: depends_on\n    target: absorb\n",
        );

        assert!(merge_pages(agent, "keep", "absorb", "2026-11-17").unwrap());

        let text = std::fs::read_to_string(
            crate::memory::pages_dir(agent)
                .join("observation")
                .join("keep.md"),
        )
        .unwrap();
        // Shared lines survive once; both bodies survive.
        assert_eq!(text.matches("shared line").count(), 1, "{text}");
        assert!(
            text.contains("the rule holds") && text.contains("the rule also holds"),
            "{text}"
        );
        // Both pieces of evidence carried over.
        assert!(
            text.contains("ref: cargo test") && text.contains("ref: '#92'"),
            "{text}"
        );
        // The absorbed page is archived, not deleted.
        assert!(!crate::memory::pages_dir(agent)
            .join("observation")
            .join("absorb.md")
            .exists());
        assert!(load_pages(agent).iter().any(|page| page.slug == "keep"));
        // The tombstone lives in the archive log, next to every other archive event.
        let log =
            std::fs::read_to_string(crate::memory::memory_dir(agent).join("_archive.md")).unwrap();
        assert!(log.contains("merged into keep"), "{log}");
        // The reference follows the survivor.
        let points = std::fs::read_to_string(
            crate::memory::pages_dir(agent)
                .join("observation")
                .join("points.md"),
        )
        .unwrap();
        assert!(points.contains("target: keep"), "{points}");
    }

    #[test]
    fn a_merge_refuses_what_it_cannot_do_safely() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        page(agent, "one", "body", "");
        page(agent, "two", "body", "");
        assert!(
            !merge_pages(agent, "one", "one", "2026-11-17").unwrap(),
            "same page"
        );
        assert!(
            !merge_pages(agent, "one", "missing", "2026-11-17").unwrap(),
            "no such page"
        );
        // A protected page is never absorbed.
        let protected = crate::memory::pages_dir(agent).join(crate::memory::MEMORY_PROTECTED_DIR);
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(
            protected.join("guard.md"),
            "---\nslug: guard\ntype: model\ntitle: guard\nstatus: active\n---\n\nbody\n",
        )
        .unwrap();
        assert!(
            !merge_pages(agent, "one", "guard", "2026-11-17").unwrap(),
            "protected"
        );
        assert!(load_pages(agent).iter().any(|page| page.slug == "two"));
    }
}
