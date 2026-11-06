//! Catalog visibility filters: the settings `modelCatalog.allow`/`modelCatalog.deny`
//! patterns hide catalog entries (`*-preview` models, retired providers, ...) from
//! every listing that sources the model catalog. Rust-only fork extension (no TS
//! counterpart at v0.9.8); the active-model selection path is unaffected.
//!
//! The match key is the structured (`provider`, `id`) pair, never slash-parsing a
//! joined selector: a pattern containing `/` binds its last segment to `model.id`
//! and the remaining segments to `model.provider`; a pattern without `/` matches
//! `model.id` only, for any provider. Pattern grammar is glob (`*`, `?`, `[...]`),
//! case-insensitive. Evaluation: a non-empty `allow` first (only allow-matching
//! models stay visible), then `deny` subtracts (deny always wins).

use std::collections::BTreeMap;
use std::path::Path;

use pa_types::ai::Model;

use crate::settings::SettingsManager;

/// The visibility filters; blank patterns behave as absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogVisibility {
    /// Allow patterns; a non-empty list keeps only matching models visible.
    pub allow: Vec<String>,
    /// Deny patterns; deny always wins over allow.
    pub deny: Vec<String>,
}

impl CatalogVisibility {
    /// Whether this instance filters anything at all (blank patterns count as absent).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active_allow().next().is_none() && self.active_deny().next().is_none()
    }

    /// One (provider, id) pair against the filters: a non-empty `allow` first,
    /// then `deny` subtracts.
    #[must_use]
    pub fn keeps(&self, provider: &str, id: &str) -> bool {
        let allow = self.active_allow().collect::<Vec<_>>();
        if !allow.is_empty() && !allow.iter().any(|p| matches(p, provider, id)) {
            return false;
        }
        !self.active_deny().any(|p| matches(p, provider, id))
    }

    fn active_allow(&self) -> impl Iterator<Item = &str> + '_ {
        self.allow
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
    }

    fn active_deny(&self) -> impl Iterator<Item = &str> + '_ {
        self.deny.iter().map(|p| p.trim()).filter(|p| !p.is_empty())
    }

    /// The filtered list plus the pattern-level summary (never model-level: the
    /// summary line must not leak selectors beyond "N hidden").
    #[must_use]
    pub fn apply(&self, models: Vec<Model>) -> FilteredModels {
        if self.is_empty() {
            return FilteredModels {
                models,
                summary: FilterSummary::default(),
            };
        }
        let mut kept = Vec::with_capacity(models.len());
        let mut hidden = 0_usize;
        let mut hidden_by_pattern = BTreeMap::new();
        for model in models {
            let (provider, id) = (&model.provider, &model.id);
            if self.keeps(provider, id) {
                kept.push(model);
            } else {
                hidden += 1;
                let pattern =
                    if let Some(hit) = self.active_deny().find(|p| matches(p, provider, id)) {
                        hit.to_string()
                    } else {
                        NOT_IN_ALLOW.to_string()
                    };
                *hidden_by_pattern.entry(pattern).or_insert(0) += 1;
            }
        }
        FilteredModels {
            models: kept,
            summary: FilterSummary {
                hidden,
                hidden_by_pattern,
            },
        }
    }
}

/// The summary label the allow filter is credited with when it hides a model.
pub const NOT_IN_ALLOW: &str = "not in allow";

/// The global-scope visibility filters for `agent_dir` (settings
/// `modelCatalog`): the catalog chain is shared per agent dir, so the
/// visibility is per-agent too (`None`/unreadable/unset = unrestricted).
#[must_use]
pub fn catalog_visibility_for_agent_dir(agent_dir: Option<&Path>) -> CatalogVisibility {
    let Some(agent_dir) = agent_dir else {
        return CatalogVisibility::default();
    };
    SettingsManager::create(agent_dir, agent_dir)
        .get_model_catalog_visibility()
        .unwrap_or_default()
}

/// The filter result for one listing: the visible entries plus the summary the
/// render seam shows next to the list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FilteredModels {
    pub models: Vec<Model>,
    pub summary: FilterSummary,
}

/// The pattern-level accounting of one filter pass (never model-level: the
/// summary line must not leak selectors beyond "N hidden").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterSummary {
    /// How many entries the filter removed from the source list.
    pub hidden: usize,
    /// Hidden count per credited pattern, sorted by pattern.
    pub hidden_by_pattern: BTreeMap<String, usize>,
}

impl FilterSummary {
    /// Whether the last pass hid nothing (no summary line is shown then).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hidden == 0
    }
}

/// One pattern against one (provider, id): glob per component, case-insensitive.
fn matches(pattern: &str, provider: &str, id: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains(['*', '?', '[']) {
        // The exact constant path keeps non-glob patterns cheap and unambiguous.
        if pattern.eq_ignore_ascii_case(id) {
            return true;
        }
        return pattern
            .rsplit_once('/')
            .is_some_and(|(provider_part, id_part)| {
                provider_part.eq_ignore_ascii_case(provider) && id_part.eq_ignore_ascii_case(id)
            });
    }
    match pattern.to_lowercase().rsplit_once('/') {
        Some((provider_pattern, id_pattern)) => {
            glob_matches(provider_pattern, provider) && glob_matches(id_pattern, id)
        }
        None => glob_matches(pattern, id),
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let Ok(glob) = globset::Glob::new(&pattern) else {
        return false;
    };
    glob.compile_matcher().is_match(value.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(allow: &[&str], deny: &[&str]) -> CatalogVisibility {
        CatalogVisibility {
            allow: allow.iter().copied().map(str::to_string).collect(),
            deny: deny.iter().copied().map(str::to_string).collect(),
        }
    }

    fn keeps(vis: &CatalogVisibility, provider: &str, id: &str) -> bool {
        vis.keeps(provider, id)
    }

    #[test]
    fn empty_filters_keep_everything() {
        assert!(CatalogVisibility::default().is_empty());
        assert!(keeps(&v(&[], &[]), "databricks", "dbrx-preview"));
    }

    #[test]
    fn deny_id_patterns_hide_any_provider() {
        let vis = v(&[], &["*-preview"]);
        assert!(!keeps(&vis, "databricks", "dbrx-preview"));
        assert!(keeps(&vis, "databricks", "dbrx"));
        // The prime-inference ids carry their own slashes: only the last
        // segment binds `id`, so `*-preview` still matches on it.
        assert!(!keeps(&vis, "prime-inference", "internal/glm-5.3-preview"));
        assert!(keeps(&vis, "prime-inference", "internal/glm-5.3"));
    }

    #[test]
    fn provider_bound_patterns_match_the_structured_pair() {
        let vis = v(&[], &["databricks/dbrx"]);
        // The relay provider ids carry slashes that must keep matching.
        assert!(!keeps(&vis, "databricks", "dbrx"));
        assert!(keeps(&vis, "databricks", "dbrx-preview"));
        assert!(keeps(&vis, "aaa/bbbb", "xxx-v-flash"));

        let vis = v(&[], &["aaa/bbbb/*"]);
        assert!(!keeps(&vis, "aaa/bbbb", "xxx-v-flash"));
        assert!(keeps(&vis, "aaa/cccc", "xxx-v-flash"));
        assert!(keeps(&vis, "aaa", "bbbb"));
    }

    #[test]
    fn allow_narrows_and_deny_wins() {
        let vis = v(&["prime-inference/*"], &["*-preview"]);
        assert!(!keeps(&vis, "databricks", "dbrx"));
        assert!(!keeps(&vis, "prime-inference", "glm-preview"));
        assert!(keeps(&vis, "prime-inference", "glm"));

        // Deny strips even an allow-matching model.
        let vis = v(&["a/*"], &["a/b"]);
        assert!(!keeps(&vis, "a", "b"));
        assert!(keeps(&vis, "a", "c"));
    }

    #[test]
    fn non_glob_patterns_match_case_insensitively() {
        let vis = v(&["Db-Provider/Db-Model"], &[]);
        assert!(keeps(&vis, "DB-PROVIDER", "db-model"));
        assert!(!keeps(&vis, "db-provider", "other"));

        let vis = v(&[], &["Databricks/DBRX"]);
        assert!(!keeps(&vis, "databricks", "dbrx"));
    }

    #[test]
    fn glob_patterns_match_case_insensitively_per_component() {
        let vis = v(&["PRIME-INFERENCE/*"], &[]);
        assert!(keeps(&vis, "prime-inference", "glm"));

        let vis = v(&[], &["Aaa/Bbbb/X-V-Flash"]);
        // The exact multi-segment form matches the structured pair.
        assert!(!keeps(&vis, "aaa/bbbb", "x-v-flash"));
        assert!(keeps(&vis, "aaa/bbbb", "y-v-flash"));

        let vis = v(&[], &["AaA/*/v-flash"]);
        // `*` in the provider part reaches across the provider's own slash
        // separators (relay provider ids carry slashes).
        assert!(!keeps(&vis, "aaa/bbbb", "v-flash"));
        assert!(!keeps(&vis, "aaa/cccc", "v-flash"));
        // A provider missing the pattern's segment does not match.
        assert!(keeps(&vis, "aaa", "v-flash"));
        assert!(keeps(&vis, "aaa/cccc", "yyy-v-flash"));
    }

    #[test]
    fn bracket_patterns_follow_the_glob_vocabulary() {
        let vis = v(&[], &["p/glm-[45]"]);
        assert!(!keeps(&vis, "p", "glm-4"));
        assert!(!keeps(&vis, "p", "glm-5"));
        assert!(keeps(&vis, "p", "glm-6"));
    }

    #[test]
    fn blank_patterns_are_treated_as_absent() {
        let vis = v(&[""], &["   "]);
        assert!(vis.is_empty());
        assert!(keeps(&vis, "p", "m"));
    }

    #[test]
    fn apply_reports_pattern_level_counts_without_leaking_selectors() {
        let models = vec![
            preview_model("databricks", "dbrx-preview"),
            preview_model("other", "x-preview"),
            settled_model("prime-inference", "glm"),
        ];
        let filtered = v(&[], &["*-preview"]).apply(models);
        assert_eq!(filtered.models.len(), 1);
        assert_eq!(filtered.summary.hidden, 2);
        assert_eq!(
            filtered.summary.hidden_by_pattern.get("*-preview"),
            Some(&2)
        );
        // No reference to the hidden entries anywhere in the summary.
        assert!(!format!("{filtered:?}").contains("dbrx"));
    }

    #[test]
    fn apply_credits_the_allow_shortfall_when_deny_never_hit() {
        let models = vec![settled_model("other", "x"), settled_model("a", "b")];
        let filtered = v(&["a/*"], &[]).apply(models);
        assert_eq!(filtered.models.len(), 1);
        assert_eq!(
            filtered.summary.hidden_by_pattern.get(NOT_IN_ALLOW),
            Some(&1)
        );
    }

    #[test]
    fn apply_with_empty_filters_is_a_no_op() {
        let models = vec![settled_model("a", "b")];
        let filtered = CatalogVisibility::default().apply(models.clone());
        assert_eq!(filtered.models, models);
        assert_eq!(filtered.summary.hidden, 0);
        assert!(filtered.summary.hidden_by_pattern.is_empty());
    }

    fn preview_model(provider: &str, id: &str) -> Model {
        settled_model(provider, id)
    }

    fn settled_model(provider: &str, id: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": "https://api.example.com/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000,
            "maxTokens": 8192,
        }))
        .unwrap()
    }
}
