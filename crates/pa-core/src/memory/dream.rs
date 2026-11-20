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

/// Record a finished pass: take the lock, append the journal line, release the
/// lock. A pass that writes its record without the lock can race another pass
/// into the same `next_at`, which is how a store ends up consolidating twice.
///
/// # Errors
///
/// Propagates the lock and append failures.
pub fn record_pass(agent_dir: &Path, record: &DreamRecord, now: u64) -> std::io::Result<bool> {
    if !acquire_lock(agent_dir, now)? {
        return Ok(false);
    }
    let appended = append_journal(agent_dir, record);
    release_lock(agent_dir);
    appended.map(|()| true)
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

/// Half-life of the read weight: a page nobody reads fades in about a month.
pub const WEIGHT_HALF_LIFE_DAYS: i64 = 30;
/// A page younger than this is never a removal candidate, however short it is:
/// the rule lives in the kernel, and this mirrors it for the walk's ordering.
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
    // Rendering the digest must not write the same file on every render: the
    // instruction only changes when the store does.
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == prompt) {
        return Ok(Some(prompt));
    }
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
4. Promote — a type is a decay bracket, not a provenance: evidence accumulates and the page moves up. Do it with `rlm.harness` or `prime-agent memory promote`, never by writing `type:` by hand. `experience` needs two independent pieces of evidence, `observation` needs two kinds of evidence, `world` needs web/file/cmd evidence a machine can re-check.\n\
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
            edges: Vec::new(),
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
