//! The full first-run flow's own surfaces: the welcome block's description
//! paragraphs and single login action, the connect-more-providers picker,
//! and the panel hosting that mounts each step's surface inside the splash
//! (the flow never nests its panels, so one slot covers the sequence).

use crate::auth_panel::PrimeTeamPick;
use crate::keybindings::KeybindingsManager;
use crate::menu_panel::search_field_plain_row;
use crate::onboarding::{highlight_wash, wrap_words, OnboardingDecision};
use crate::onboarding_choice::OnboardingChoice;
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};
use ratatui::style::Modifier;
use tokio::sync::oneshot;

pub(crate) const PROVIDERS_PROMPT: &str = "Connect other providers, or continue.";
pub(crate) const PROVIDERS_SEARCH_PLACEHOLDER: &str = "Search providers";
pub(crate) const PROVIDERS_NOTE: &str = "You can add providers anytime with /login.";
pub(crate) const CONTINUE_LABEL: &str = "Continue";
pub(crate) const API_KEY_PROMPT: &str = "Enter API key:";

const WELCOME_DESCRIPTION_PARAGRAPHS: [&str; 2] = [
    "A self-improving RLM harness with persistent context, recursive subagents, and direct swarm communication.",
    "It learns from its history by refining its own memories, skills, prompts, and subagent specifications.",
];
const WELCOME_DESCRIPTION_WIDTH: usize = 56;

const MARKER_WIDTH: usize = 2;
const MIN_ROW_WIDTH: usize = 34;
const ROW_TRAILING: usize = 6;
const VISIBLE_ROWS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPickerOption {
    pub id: String,
    pub name: String,
    /// Already signed in: the row is marked with a check rather than a note.
    pub connected: bool,
    /// Whether this build carries the row's login flow (the /login menu rule:
    /// an unavailable row is dimmed, Enter inert).
    pub available: bool,
}

#[derive(Debug, Clone, Copy)]
struct RowMarks {
    connected: bool,
    selected: bool,
    available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderPick {
    /// The pinned continue row.
    Continue,
    /// One provider row, by id.
    Provider(String),
    /// Esc: the step ends (TS `onCancel` → the flow continues).
    Cancelled,
}

/// A searchable list in the onboarding block: the continue action pinned at index 0, the matching
/// rows under it in a scrolling viewport, and the below/top hints. Reused for both the welcome
/// provider list and the post-auth model list.
#[derive(Debug)]
pub struct ProviderPicker {
    options: Vec<ProviderPickerOption>,
    search: SearchInput,
    /// The selected row: 0 is the continue action, 1.. the visible rows.
    selected: usize,
    scroll_top: usize,
    /// The pinned row's label.
    continue_label: String,
    /// The prompt line above the search field.
    prompt: String,
    /// Picker-relative row -> selection index, recorded at render time for mouse clicks.
    row_hits: std::cell::RefCell<Vec<(usize, usize)>>,
}

impl ProviderPicker {
    #[must_use]
    pub fn new(options: Vec<ProviderPickerOption>) -> Self {
        ProviderPicker {
            options,
            search: SearchInput::new(),
            selected: 0,
            scroll_top: 0,
            continue_label: CONTINUE_LABEL.to_string(),
            prompt: PROVIDERS_PROMPT.to_string(),
            row_hits: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The welcome-screen variant: the pinned row says "Continue without signing in" and the
    /// prompt invites the first provider choice rather than "connect more".
    #[must_use]
    pub fn welcome(options: Vec<ProviderPickerOption>) -> Self {
        let mut picker = Self::new(options);
        picker.continue_label = "Continue without signing in".to_string();
        picker.prompt = "Choose a provider to start, or continue without signing in.".to_string();
        picker
    }

    /// The post-auth model-list variant: the pinned row goes back to the provider list.
    #[must_use]
    pub fn model_picker(options: Vec<ProviderPickerOption>) -> Self {
        let mut picker = Self::new(options);
        picker.continue_label = "Back".to_string();
        picker.prompt = "Choose a model for this provider.".to_string();
        picker
    }

    /// One wheel turn: positive scrolls down, negative up. The selection moves and the viewport
    /// follows it, as the arrow keys do.
    pub fn handle_wheel(&mut self, delta: isize) {
        let steps = delta.unsigned_abs();
        let direction = if delta < 0 { -1 } else { 1 };
        for _ in 0..steps {
            self.move_selection(direction);
        }
    }

    /// A left click on the row at `row` (picker-relative, as recorded by the last render). The
    /// first click selects the row; a click on the already-selected row confirms it, like Enter.
    pub fn handle_mouse_click(&mut self, row: usize) -> Option<ProviderPick> {
        let index = self
            .row_hits
            .borrow()
            .iter()
            .find(|(line, _)| *line == row)
            .map(|(_, index)| *index)?;
        if self.selected == index {
            return self.confirm_selection();
        }
        self.selected = index;
        None
    }

    /// One key id (the splash answers the exit keys first); `None` keeps the
    /// picker mounted.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> Option<ProviderPick> {
        if kb.matches(key, "tui.select.up") {
            self.move_selection(-1);
            return None;
        }
        if kb.matches(key, "tui.select.down") {
            self.move_selection(1);
            return None;
        }
        if kb.matches(key, "tui.select.confirm") || self.arrow_confirms(key) {
            return self.confirm_selection();
        }
        if kb.matches(key, "tui.select.cancel") || self.arrow_cancels(key) {
            return Some(ProviderPick::Cancelled);
        }
        self.search.handle_key(key, kb);
        self.selected = self.selected.min(self.filtered().len());
        self.scroll_top = 0;
        None
    }

    /// One paste payload: the paste lands in the query, and the filter's
    /// re-clamp keeps the selection inside the match list.
    pub fn handle_paste(&mut self, text: &str) {
        // A paste is external bytes: the control-sequence scrub runs before
        // the query renders.
        self.search.paste(&crate::menu_panel::scrub_controls(text));
        self.selected = self.selected.min(self.filtered().len());
        self.scroll_top = 0;
    }

    /// The tray hint: the confirm and back keys by their bound labels. The arrows join the
    /// labels only while the query is empty, when they act as confirm and back.
    #[must_use]
    pub fn hint_text(&self, kb: &KeybindingsManager) -> String {
        let arrows = self.search.value().is_empty();
        let mut segments = Vec::new();
        if let Some(enter) = kb.first_key("tui.select.confirm") {
            let mut label = crate::keybindings::format_key_text(&enter);
            if arrows {
                label.push_str("/\u{2192}");
            }
            segments.push(format!("{label} select"));
        }
        if let Some(escape) = kb.first_key("tui.select.cancel") {
            let mut label = crate::keybindings::format_key_text(&escape);
            if arrows {
                label.push_str("/\u{2190}");
            }
            segments.push(format!("{label} later"));
        }
        segments.join("   ")
    }

    /// Right confirms the selection, but only while the query is empty (a non-empty query keeps
    /// the arrows on the search caret).
    fn arrow_confirms(&self, key: &str) -> bool {
        key == "right" && self.search.value().is_empty()
    }

    /// Left cancels only while the query is empty, like the model picker's back arrow.
    fn arrow_cancels(&self, key: &str) -> bool {
        key == "left" && self.search.value().is_empty()
    }

    /// Enter's action: the pinned continue row or the selected provider row.
    fn confirm_selection(&self) -> Option<ProviderPick> {
        if self.selected == 0 {
            return Some(ProviderPick::Continue);
        }
        let filtered = self.filtered();
        let item = filtered.get(self.selected - 1)?;
        // The /login menu rule: an unavailable row's Enter is inert.
        if !item.available {
            return None;
        }
        Some(ProviderPick::Provider(item.id.clone()))
    }

    /// The query's matches: the lowercased trimmed query matches the label or the id.
    fn filtered(&self) -> Vec<&ProviderPickerOption> {
        let query = self.search.value().trim().to_lowercase();
        if query.is_empty() {
            return self.options.iter().collect();
        }
        self.options
            .iter()
            .filter(|option| {
                option.name.to_lowercase().contains(&query)
                    || option.id.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// The selection never leaves `0..=len`; the viewport follows it.
    fn move_selection(&mut self, delta: i32) {
        let filtered = self.filtered().len();
        let next = self.selected as i64 + i64::from(delta);
        if next < 0 || next > filtered as i64 {
            return;
        }
        self.selected = next as usize;
        if self.selected >= 1 {
            let item_index = self.selected - 1;
            if item_index < self.scroll_top {
                self.scroll_top = item_index;
            } else if item_index >= self.scroll_top + VISIBLE_ROWS {
                self.scroll_top = item_index - VISIBLE_ROWS + 1;
            }
        }
    }

    /// The picker's frame: the prompt, the search field, the pinned continue row,
    /// the viewport's rows, the below/top hint, and the trailing note.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize) -> Vec<Line> {
        let width = width.max(1);
        let filtered = self.filtered();
        let max_scroll = filtered.len().saturating_sub(VISIBLE_ROWS);
        let scroll_top = self.scroll_top.min(max_scroll);
        let row_width = self.row_width(width);
        let mut hits: Vec<(usize, usize)> = Vec::new();
        let mut lines: Vec<Line> = vec![padded(width)];
        lines.push(indented(theme, width, &self.prompt, ThemeColor::Text));
        lines.push(padded(width));
        lines.push(search_field_plain_row(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            true,
            PROVIDERS_SEARCH_PLACEHOLDER,
        ));
        lines.push(padded(width));
        hits.push((lines.len(), 0));
        lines.push(Self::row(
            theme,
            width,
            row_width,
            &self.continue_label,
            RowMarks {
                connected: false,
                selected: self.selected == 0,
                available: true,
            },
        ));
        let end = (scroll_top + VISIBLE_ROWS).min(filtered.len());
        for (index, item) in filtered.iter().enumerate().take(end).skip(scroll_top) {
            // The menu rule: an unavailable row's dimmed label carries the annotation.
            let label = if item.available {
                item.name.clone()
            } else {
                format!("{} · not available", item.name)
            };
            hits.push((lines.len(), index + 1));
            lines.push(Self::row(
                theme,
                width,
                row_width,
                &label,
                RowMarks {
                    connected: item.connected,
                    selected: self.selected == index + 1,
                    available: item.available,
                },
            ));
        }
        *self.row_hits.borrow_mut() = hits;
        let remaining = filtered.len() - end;
        if remaining > 0 || scroll_top > 0 {
            let hint = if remaining > 0 {
                format!("{remaining} more below")
            } else {
                "top of list".to_string()
            };
            lines.push(indented(
                theme,
                width,
                &format!("  {hint}"),
                ThemeColor::Dim,
            ));
        }
        lines.push(padded(width));
        lines.push(indented(theme, width, PROVIDERS_NOTE, ThemeColor::Dim));
        lines
    }

    /// One row: the marker, the label, the connected check, and the row-width
    /// padding; the selected row carries the highlight wash.
    fn row(theme: &Theme, width: usize, row_width: usize, label: &str, marks: RowMarks) -> Line {
        let RowMarks {
            connected,
            selected,
            available,
        } = marks;
        let name = format!("{}{}", if selected { "> " } else { "  " }, label);
        let mark = if connected { "  \u{2713}" } else { "" };
        let pad = " ".repeat(
            row_width
                .saturating_sub(crate::width::str_width(&name) + crate::width::str_width(mark)),
        );
        let wash = highlight_wash(theme);
        let mut line: Line = vec![Span::raw(" ")];
        // The menu rule: an unavailable row's label stays dim even when selected.
        let label_color = if available {
            ThemeColor::Text
        } else {
            ThemeColor::Muted
        };
        if selected {
            // The selected row lifts off the canvas: a bold name, the success
            // check, and the padding all washed.
            let mut washed_name = Span::styled(
                name,
                theme.fg_style(label_color).add_modifier(Modifier::BOLD),
            );
            washed_name.style = washed_name.style.bg(wash);
            line.push(washed_name);
            if connected {
                let mut washed_mark = Span::styled(mark, theme.fg_style(ThemeColor::Success));
                washed_mark.style = washed_mark.style.bg(wash);
                line.push(washed_mark);
            }
            let mut washed_pad = Span::styled(pad, theme.fg_style(ThemeColor::Dim));
            washed_pad.style = washed_pad.style.bg(wash);
            line.push(washed_pad);
        } else {
            line.push(Span::styled(name, theme.fg_style(ThemeColor::Muted)));
            if connected {
                line.push(Span::styled(mark, theme.fg_style(ThemeColor::Success)));
            }
            line.push(Span::styled(pad, theme.fg_style(ThemeColor::Dim)));
        }

        let line = crate::width::truncate_line(&line, width, "");
        pad_to(line, width)
    }

    /// The longest label (plus its check) under the width budget, floored at the minimum.
    fn row_width(&self, width: usize) -> usize {
        let longest = self
            .options
            .iter()
            .map(|option| {
                crate::width::str_width(&option.name) + if option.connected { 3 } else { 0 }
            })
            .max()
            .unwrap_or(0);
        (MARKER_WIDTH + longest + ROW_TRAILING)
            .max(MIN_ROW_WIDTH)
            .min(width.saturating_sub(1).max(1))
    }
}

#[derive(Debug)]
pub enum OnboardingPanel {
    /// A login flow's inline auth panel with the heading line that replaces the brand mark while it
    /// owns the block. The dialog is boxed: it dwarfs the other variants.
    Auth {
        panel: std::boxed::Box<crate::auth_panel::AuthPanel>,
        heading: Option<String>,
    },
    /// The connect-more-providers picker.
    Providers(ProviderPicker),
    /// The trace question (TS `askOnboardingTraceOptIn`'s choice).
    Question(OnboardingChoice),
    /// The Prime team question (no heading, so the brand line returns). The pick answers the flow's
    /// `SelectTeam` request directly: the login flow settles through its own future.
    TeamQuestion {
        choice: OnboardingChoice,
        teams: Vec<crate::auth_panel::PrimeTeamOption>,
        reply: Option<oneshot::Sender<PrimeTeamPick>>,
    },
}

impl OnboardingPanel {
    /// The panel that owns the block names itself; `None` keeps the brand line.
    #[must_use]
    pub fn heading(&self) -> Option<&str> {
        match self {
            OnboardingPanel::Auth { heading, .. } => heading.as_deref(),
            _ => None,
        }
    }

    /// The panel's rows (the panel indents its own content); the auth panel's hint row renders the
    /// effective bindings, so the keybindings manager rides along.
    pub(crate) fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
    ) -> Vec<Line> {
        match self {
            OnboardingPanel::Auth { panel, .. } => panel.render(theme, width, kb),
            OnboardingPanel::Providers(picker) => picker.render(theme, width),
            OnboardingPanel::Question(choice) | OnboardingPanel::TeamQuestion { choice, .. } => {
                choice.render(theme, width)
            }
        }
    }

    /// One key (`None` keeps the pane waiting); `osc_sink` carries the login
    /// dialog's URL-copy OSC 52 fallback.
    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        kb: &KeybindingsManager,
        osc_sink: &mut crate::clipboard::OscSink,
    ) -> Option<OnboardingDecision> {
        match self {
            // The dialog consumes every key itself; the flow behind it settles
            // through its own future.
            OnboardingPanel::Auth { panel, .. } => {
                panel.handle_key(key, kb, osc_sink);
                None
            }
            OnboardingPanel::Providers(picker) => {
                picker.handle_key(key, kb).map(OnboardingDecision::Pick)
            }
            OnboardingPanel::Question(choice) => {
                if kb.matches(key, "tui.select.cancel") {
                    return Some(OnboardingDecision::Cancelled);
                }
                if kb.matches(key, "tui.select.up") {
                    choice.move_selection(-1);
                    return None;
                }
                if kb.matches(key, "tui.select.down") {
                    choice.move_selection(1);
                    return None;
                }
                if kb.matches(key, "tui.select.confirm") {
                    return Some(OnboardingDecision::Selected(choice.selected()));
                }
                None
            }
            // The choice answers its own oneshot (index 0 the personal account, 1.. the teams; a
            // cancel keeps the stored selection).
            OnboardingPanel::TeamQuestion {
                choice,
                teams,
                reply,
            } => {
                if kb.matches(key, "tui.select.cancel") {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(PrimeTeamPick::Cancelled);
                    }
                    return None;
                }
                if kb.matches(key, "tui.select.up") {
                    choice.move_selection(-1);
                    return None;
                }
                if kb.matches(key, "tui.select.down") {
                    choice.move_selection(1);
                    return None;
                }
                if kb.matches(key, "tui.select.confirm") {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Self::choice_pick(choice.selected(), teams));
                    }
                }
                None
            }
        }
    }

    /// One paste payload: the login dialog's field, or the picker's search —
    /// the questions have no input.
    pub fn handle_paste(&mut self, text: &str) {
        match self {
            OnboardingPanel::Auth { panel, .. } => panel.handle_paste(text),
            OnboardingPanel::Providers(picker) => picker.handle_paste(text),
            OnboardingPanel::Question(_) | OnboardingPanel::TeamQuestion { .. } => {}
        }
    }

    /// The team question's answer: index 0 the personal account, 1.. the team
    /// row — a row past the list answers the personal account.
    fn choice_pick(selected: usize, teams: &[crate::auth_panel::PrimeTeamOption]) -> PrimeTeamPick {
        if selected == 0 {
            return PrimeTeamPick::PersonalAccount;
        }
        teams
            .get(selected - 1)
            .cloned()
            .map_or(PrimeTeamPick::PersonalAccount, PrimeTeamPick::Team)
    }
}

/// The welcome block under the brand line: the wrapped description paragraphs with a blank row
/// between them, and the trailing blank that separates them from the action.
pub(crate) fn welcome_rows(theme: &Theme, width: usize) -> Vec<Line> {
    let wrap = WELCOME_DESCRIPTION_WIDTH
        .min(width.saturating_sub(1))
        .max(1);
    let mut lines: Vec<Line> = vec![padded(width)];
    for (index, paragraph) in WELCOME_DESCRIPTION_PARAGRAPHS.iter().enumerate() {
        if index > 0 {
            lines.push(padded(width));
        }
        for row in wrap_words(paragraph, wrap) {
            lines.push(indented(theme, width, &row, ThemeColor::Muted));
        }
    }
    lines.push(padded(width));
    lines
}

fn indented(theme: &Theme, width: usize, text: &str, tone: ThemeColor) -> Line {
    pad_to(vec![Span::raw(" "), theme.fg_span(tone, text)], width)
}

fn padded(width: usize) -> Line {
    pad_to(Vec::new(), width)
}

fn pad_to(mut line: Line, width: usize) -> Line {
    let used = crate::width::spans_width(&line);
    if used < width {
        line.push(Span::raw(" ".repeat(width - used)));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn options(count: usize) -> Vec<ProviderPickerOption> {
        (0..count)
            .map(|index| ProviderPickerOption {
                id: format!("provider-{index}"),
                name: format!("Provider {index}"),
                connected: index == 0,
                available: true,
            })
            .collect()
    }

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn row_text(line: &Line) -> String {
        line.iter().map(|span| span.content.clone()).collect()
    }

    fn row_of(rows: &[Line], label: &str) -> Option<usize> {
        rows.iter().position(|row| row_text(row).contains(label))
    }

    #[test]
    fn the_welcome_paragraphs_wrap_at_the_ts_width() {
        let rows = welcome_rows(&theme(), 80);
        assert_eq!(rows.len(), 7);
        let text: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            text.iter().any(|row| row.contains("persistent context")),
            "the first paragraph renders: {text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("refining its own")),
            "the second paragraph wraps at the TS width: {text:?}"
        );
    }

    #[test]
    fn the_picker_pins_the_continue_row_and_marks_connected() {
        let picker = ProviderPicker::new(vec![
            ProviderPickerOption {
                id: "one".to_string(),
                name: "One".to_string(),
                connected: true,
                available: true,
            },
            ProviderPickerOption {
                id: "two".to_string(),
                name: "Two".to_string(),
                connected: false,
                available: true,
            },
        ]);
        let rows = picker.render(&theme(), 60);
        let text: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            text.iter().any(|row| row.contains(CONTINUE_LABEL)),
            "the continue row: {text:?}"
        );
        assert!(
            text.iter()
                .any(|row| row.contains("One") && row.contains('\u{2713}'.to_string().as_str())),
            "the connected check rides its row: {text:?}"
        );
    }

    /// The /login menu rule in the picker: an unavailable row is dimmed and
    /// Enter is inert — no after-selection error wall.
    #[test]
    fn the_picker_marks_unavailable_rows_inert() {
        let mut picker = ProviderPicker::new(vec![ProviderPickerOption {
            id: "anthropic".to_string(),
            name: "Anthropic".to_string(),
            connected: false,
            available: false,
        }]);
        let rows = picker.render(&theme(), 60);
        let text: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            text.iter()
                .any(|row| row.contains("Anthropic · not available")),
            "the unavailable row carries the inline annotation: {text:?}"
        );
        // Enter on the unavailable row keeps the picker mounted.
        picker.handle_key("down", &kb());
        assert!(
            picker.handle_key("enter", &kb()).is_none(),
            "Enter on an unavailable row is inert"
        );
    }

    fn key_kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    #[test]
    fn right_arrow_confirms_like_enter_when_the_search_is_empty() {
        let mut picker = ProviderPicker::new(options(1));
        let kb = key_kb();
        picker.handle_key("down", &kb);
        assert_eq!(
            picker.handle_key("right", &kb),
            Some(ProviderPick::Provider("provider-0".to_string()))
        );
        let mut picker = ProviderPicker::welcome(options(1));
        assert_eq!(
            picker.handle_key("right", &kb),
            Some(ProviderPick::Continue),
            "Right on the pinned continue row confirms it"
        );
    }

    #[test]
    fn left_arrow_cancels_when_the_search_is_empty() {
        let mut picker = ProviderPicker::welcome(options(1));
        assert_eq!(
            picker.handle_key("left", &key_kb()),
            Some(ProviderPick::Cancelled)
        );
    }

    #[test]
    fn left_and_right_edit_the_query_while_it_has_text() {
        let mut picker = ProviderPicker::welcome(options(3));
        let kb = key_kb();
        picker.handle_key("p", &kb);
        assert_eq!(picker.handle_key("left", &kb), None, "Left moves the caret");
        assert_eq!(
            picker.handle_key("right", &kb),
            None,
            "Right moves the caret"
        );
        assert_eq!(picker.search.value(), "p", "the query is unchanged");
    }

    #[test]
    fn the_wheel_scrolls_the_viewport_and_moves_the_selection() {
        let mut picker = ProviderPicker::welcome(options(10));
        let kb = key_kb();
        for _ in 0..8 {
            picker.handle_wheel(1);
        }
        assert_eq!(
            picker.handle_key("enter", &kb),
            Some(ProviderPick::Provider("provider-7".to_string())),
            "eight wheel-downs from the continue row select the eighth provider"
        );
        let mut picker = ProviderPicker::welcome(options(10));
        for _ in 0..8 {
            picker.handle_wheel(1);
        }
        let rows = picker.render(&theme(), 60);
        let text: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            text.iter().any(|row| row.contains("Provider 7")),
            "the selected row stays in the viewport: {text:?}"
        );
        assert!(
            !text.iter().any(|row| row.contains("Provider 0 ")),
            "the first rows scrolled out: {text:?}"
        );
    }

    #[test]
    fn a_left_click_selects_the_row_under_the_pointer() {
        let mut picker = ProviderPicker::welcome(options(3));
        let kb = key_kb();
        let rows = picker.render(&theme(), 60);
        let row = row_of(&rows, "Provider 1").expect("row 1 is rendered");
        assert_eq!(
            picker.handle_mouse_click(row),
            None,
            "a first click selects"
        );
        assert_eq!(
            picker.handle_key("enter", &kb),
            Some(ProviderPick::Provider("provider-1".to_string())),
            "the clicked row is the selection"
        );
    }

    #[test]
    fn a_second_click_on_the_selected_row_confirms_it() {
        let mut picker = ProviderPicker::welcome(options(3));
        let rows = picker.render(&theme(), 60);
        let row = row_of(&rows, "Provider 0").expect("row 0 is rendered");
        assert_eq!(picker.handle_mouse_click(row), None);
        assert_eq!(
            picker.handle_mouse_click(row),
            Some(ProviderPick::Provider("provider-0".to_string()))
        );
    }

    #[test]
    fn the_picker_answers_the_three_row_kinds() {
        let mut picker = ProviderPicker::new(options(1));
        let kb = kb();
        // Enter on the pinned continue row.
        assert_eq!(
            picker.handle_key("enter", &kb),
            Some(ProviderPick::Continue)
        );
        // Down to the provider row: Enter answers its id.
        picker.handle_key("down", &kb);
        assert_eq!(
            picker.handle_key("enter", &kb),
            Some(ProviderPick::Provider("provider-0".to_string()))
        );
        // Esc ends the step.
        assert_eq!(
            picker.handle_key("escape", &kb),
            Some(ProviderPick::Cancelled)
        );
    }

    #[test]
    fn a_typed_query_filters_the_rows_and_resets_the_scroll() {
        let mut picker = ProviderPicker::new(options(10));
        let kb = kb();
        // Scroll down past the viewport, then type a matching query: the filter
        // re-clamps the selection and resets the scroll to the top.
        for _ in 0..9 {
            picker.handle_key("down", &kb);
        }
        picker.handle_key("7", &kb);
        let rows = picker.render(&theme(), 60);
        let text: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            text.iter().any(|row| row.contains("Provider 7")),
            "the match renders: {text:?}"
        );
        assert!(
            !text.iter().any(|row| row.contains("Provider 0")),
            "the filter hides the non-matches: {text:?}"
        );
    }
}
