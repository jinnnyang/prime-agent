//! The first-run onboarding concern: the sink and readiness seams, the flow task, the pane drive
//! over the mounted onboarding screens, and the phase that runs the flow before the session screen.

use super::{
    mpsc, AgentView, Duration, ExitGuard, Future, Instant, KeybindingsManager, Pin, Renderer,
    Result, SessionUi, UiInput,
};

/// Persistence for the first-run onboarding answers. The TUI crate owns only the surface; the
/// composition root (pa-cli) implements the sink, keeping pa-tui decoupled from pa-core.
pub trait OnboardingSink: Send + Sync {
    /// The completion marker, read fresh: the phase's one-shot gate — the agents-view flow
    /// re-runs the phase per session, so a completed flow never shows anything again.
    fn onboarding_shown(&self) -> bool;
    /// Mark the onboarding flow completed; an aborted flow leaves the flag unset.
    ///
    /// # Errors
    ///
    /// Returns `Err` when persisting the completion marker fails.
    fn mark_onboarding_complete(&self) -> anyhow::Result<()>;
    /// The flow ran but did not complete (TS `runStartupOnboarding`'s
    /// `finally`): `outcome` is `aborted` (cancelled, quit, or no usable
    /// model at the end) or `error` (the flow failed). Resolves once the
    /// report is delivered (or dropped), so a quit right after cannot
    /// tear the runtime down under it.
    fn onboarding_incomplete(
        &self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The model-readiness probe: the composition root re-resolves the startup model chain, because the
/// flow's own steps can change the answer (the Prime sign-in configures the startup model).
pub type ModelReadiness = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// The first-run flow to run before the session screen: the model-ready branch asks the trace
/// question on the immediate splash; a home with no usable model runs the full flow — the
/// welcome login, the Prime Inference sign-in, the default-model apply, the
/// connect-more-providers picker, and the trace question.
#[derive(Clone)]
pub struct OnboardingTask {
    pub sink: std::sync::Arc<dyn OnboardingSink>,
    pub model_ready: ModelReadiness,
    pub current_model: Option<pa_types::ai::Model>,
    /// The provider auth flows the full branch signs in through; `None` skips its sign-in step.
    pub provider_auth: Option<crate::provider_auth::ProviderAuthCommandsHandle>,
}

impl std::fmt::Debug for OnboardingTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnboardingTask").finish()
    }
}

/// One onboarding flow's background task: the spawned join plus the flow's cooperative cancel
/// signal, shared with the panel handle.
struct OnboardingFlowTask {
    join: tokio::task::JoinHandle<crate::provider_auth::ProviderAuthOutcome>,
    cancel: crate::auth_panel::FlowCancel,
}

impl OnboardingFlowTask {
    /// Spawn the flow and pair it with the panel handle's cancel signal: the blocking login body
    /// checks the signal before its auth-store writes (a `JoinHandle::abort` cannot reach a started
    /// `spawn_blocking` login).
    fn spawn<F>(future: F, cancel: crate::auth_panel::FlowCancel) -> Self
    where
        F: std::future::Future<Output = crate::provider_auth::ProviderAuthOutcome> + Send + 'static,
    {
        OnboardingFlowTask {
            join: tokio::spawn(future),
            cancel,
        }
    }

    fn settle(
        &mut self,
    ) -> &mut tokio::task::JoinHandle<crate::provider_auth::ProviderAuthOutcome> {
        &mut self.join
    }

    /// End the flow with the exiting pane: mark the cooperative signal, then wait for the blocking
    /// login to observe it (bounded by the login's request timeouts).
    async fn end(self) {
        self.cancel.mark();
        let _ = self.join.await;
    }
}

enum PaneOutcome {
    Decision(crate::onboarding::OnboardingDecision),
    /// The input channel closed: the flow ends without an answer and the marker stays unset.
    InputClosed,
    /// The flow settled; `Err` is a crashed task.
    Flow(Result<crate::provider_auth::ProviderAuthOutcome, tokio::task::JoinError>),
}

/// The pane drive's borrowed services.
pub(super) struct PaneDrive<'a> {
    pub(super) ui_rx: &'a mut mpsc::UnboundedReceiver<UiInput>,
    pub(super) renderer: &'a mut Renderer,
    pub(super) exit_guard: &'a ExitGuard,
    pub(super) keybindings: KeybindingsManager,
    pub(super) auth_panel_rx: &'a mut mpsc::UnboundedReceiver<crate::auth_panel::AuthPanelRequest>,
    /// The run loop's headless-plan-completed flag: the pane marks it when the plan's
    /// `HeadlessDone` lands while it owns the input channel, so the run loop's idle gate still ends
    /// the run.
    pub(super) headless_done: &'a mut bool,
}

/// The pane drive's render barrier (the run loop's barrier contract): condition steps hold the
/// queued input batch behind them until a frame rendered after arming satisfies the condition.
enum PaneBarrier {
    /// `WaitRender`: a frame rendered at or after the baseline contains the needle.
    Render {
        needle: String,
        baseline: usize,
        deadline: Instant,
    },
    /// `WaitGone`: the newest frame no longer contains the needle.
    Gone { needle: String, deadline: Instant },
}

/// Draw the mounted onboarding screen and drive it until a key decides, the exit keys quit,
/// or the optional background flow settles. Each iteration draws first and waits after — a
/// deciding key that is already queued still leaves the mounted frame captured.
async fn drive_onboarding_pane(
    view: &mut AgentView,
    drive: &mut PaneDrive<'_>,
    mut screen: crate::onboarding::OnboardingScreen,
    mut flow: Option<OnboardingFlowTask>,
    osc_sink: &mut crate::clipboard::OscSink,
) -> Result<(crate::onboarding::OnboardingScreen, PaneOutcome)> {
    let mut barrier: Option<PaneBarrier> = None;
    loop {
        if let Some(armed) = barrier.take() {
            let satisfied = drive
                .renderer
                .headless_frames()
                .is_some_and(|frames| match &armed {
                    PaneBarrier::Render {
                        needle, baseline, ..
                    } => frames
                        .get(*baseline..)
                        .unwrap_or_default()
                        .iter()
                        .any(|frame| frame.contains(needle.as_str())),
                    PaneBarrier::Gone { needle, .. } => !frames
                        .last()
                        .is_some_and(|frame| frame.contains(needle.as_str())),
                });
            let expired = match &armed {
                PaneBarrier::Render { deadline, .. } | PaneBarrier::Gone { deadline, .. } => {
                    Instant::now() > *deadline
                }
            };
            if !satisfied && !expired {
                barrier = Some(armed);
            }
        }
        view.onboarding = Some(screen);
        match drive.renderer {
            Renderer::Terminal { .. } => {
                if let Some(renderer) = drive.renderer.is_terminal_mut() {
                    if let Err(error) = crate::app::draw(renderer, view) {
                        // A failed frame ends the pane: end a still-running login flow with it
                        // (the cooperative cancel reaches the blocking login body, so no path
                        // leaves a detached flow writing credentials).
                        if let Some(task) = flow.take() {
                            task.end().await;
                        }
                        return Err(error);
                    }
                }
            }
            Renderer::Headless { .. } => drive.renderer.render_headless_pane(view),
        }
        let Some(mut pane) = view.onboarding.take() else {
            unreachable!("the pane mounts at the top of every iteration");
        };
        tokio::select! {
            maybe_input = drive.ui_rx.recv(), if barrier.is_none() => {
                // Without this arm the always-ready `recv()` spins the redraw loop hot.
                let Some(input) = maybe_input else {
                    // The cooperative cancel reaches the blocking login body.
                    if let Some(task) = flow.take() {
                        task.end().await;
                    }
                    return Ok((pane, PaneOutcome::InputClosed));
                };
                match input {
                UiInput::Key(key) => {
                    let Some(key_id) = crate::keys::key_event_to_id(&key) else {
                        screen = pane;
                        continue;
                    };
                    // The onboarding exit keys include Ctrl+C: report the handled press so the
                    // force-quit guard's handled counter stays in sync with the reader's
                    // observations.
                    if key_id == "ctrl+c" {
                        drive.exit_guard.note_ctrl_c_handled();
                    }
                    if let Some(decision) = pane.handle_key(&key_id, &drive.keybindings, osc_sink) {
                        // A decision tears the pane down mid-drive: end a still-running login flow.
                        if let Some(task) = flow.take() {
                            task.end().await;
                        }
                        return Ok((pane, PaneOutcome::Decision(decision)));
                    }
                }
                UiInput::Paste(text) => {
                    pane.handle_paste(&text);
                }
                UiInput::Mouse(event) => {
                    if let Some(decision) = pane.handle_mouse(&event) {
                        if let Some(task) = flow.take() {
                            task.end().await;
                        }
                        return Ok((pane, PaneOutcome::Decision(decision)));
                    }
                }
                // The headless plan completed while the pane owned the channel: mark the run loop's
                // flag (the pane keeps driving until the channel closes or a decision ends it).
                UiInput::HeadlessDone => *drive.headless_done = true,
                UiInput::Timestamp(_) => {
                    if let Some(task) = flow.take() {
                        task.end().await;
                    }
                    anyhow::bail!("headless timing markers require an attached session");
                }
                // The plan's render barriers (pane-scoped): a condition that already holds pops
                // immediately; a pending one arms and holds the input batch behind it until a later
                // frame satisfies it or the deadline pops (the timeout proceeds silently — the
                // harness's assertion then reports the actual frame). The steps only ever come from
                // the headless harness; a terminal pane consumes them as no-ops.
                UiInput::WaitRender { needle, timeout_ms } => {
                    let holds_now = drive.renderer.headless_frames().is_some_and(|frames| {
                        frames.last().is_some_and(|frame| frame.contains(needle.as_str()))
                    });
                    if !holds_now {
                        if let Some(frames) = drive.renderer.headless_frames() {
                            barrier = Some(PaneBarrier::Render {
                                needle,
                                baseline: frames.len(),
                                deadline: Instant::now()
                                    + Duration::from_millis(timeout_ms),
                            });
                        }
                    }
                }
                UiInput::WaitGone { needle, timeout_ms } => {
                    let holds_now = drive.renderer.headless_frames().is_some_and(|frames| {
                        !frames.last().is_some_and(|frame| frame.contains(needle.as_str()))
                    });
                    if !holds_now {
                        barrier = Some(PaneBarrier::Gone {
                            needle,
                            deadline: Instant::now()
                                + Duration::from_millis(timeout_ms),
                        });
                    }
                }
                UiInput::Submit(_)
                | UiInput::SubmitAndSettle { .. }
                | UiInput::SettleIdle
                | UiInput::InputChunkEnd
                | UiInput::WaitIdle { .. }
                | UiInput::ScrollTop
                | UiInput::Resize => {}
                }
            }
            maybe_request = drive.auth_panel_rx.recv() => {
                if let Some(request) = maybe_request {
                    pane.apply_auth_request(request);
                }
            }
            settled = async {
                match flow.as_mut() {
                    Some(task) => task.settle().await,
                    None => std::future::pending().await,
                }
            } => {
                return Ok((pane, PaneOutcome::Flow(settled)));
            }
            () = tokio::time::sleep(Duration::from_millis(120)) => {
                pane.tick();
            }
        }
        screen = pane;
    }
}

/// Drive the first-run onboarding flow before the session screen (TS `runStartupOnboarding`).
/// Returns `true` when the exit keys quit the app.
pub(super) async fn run_onboarding_phase(
    task: &OnboardingTask,
    session: &mut SessionUi,
    view: &mut AgentView,
    drive: &mut PaneDrive<'_>,
) -> Result<bool> {
    // One-shot: the agents-view flow re-runs the phase for every session it opens with the
    // same task. A flow that already completed (the marker now persisted) re-checks here
    // and never shows anything again.
    if task.sink.onboarding_shown() {
        return Ok(false);
    }
    let result = run_onboarding_flow(task, session, view, drive).await;
    // Only a completed flow sets the marker; every other ending reports
    // its outcome (TS `onboarding completed` with `aborted` / `error`).
    if !task.sink.onboarding_shown() {
        task.sink
            .onboarding_incomplete(if result.is_err() { "error" } else { "aborted" })
            .await;
    }
    result
}

async fn run_onboarding_flow(
    task: &OnboardingTask,
    session: &mut SessionUi,
    view: &mut AgentView,
    drive: &mut PaneDrive<'_>,
) -> Result<bool> {
    // The ready branch: a home that already has a usable model completes silently. The trace
    // opt-in question was removed from onboarding — users can enable it later via /traces.
    if (task.model_ready)() {
        if let Err(error) = task.sink.mark_onboarding_complete() {
            warn_onboarding_persist_failure(session, view, &error);
        }
        return Ok(false);
    }

    let Some(provider_auth) = task.provider_auth.clone() else {
        return Ok(false);
    };

    // The not-ready loop: welcome picker → provider auth → model picker, until the user completes
    // a flow, picks "Continue without signing in", or presses Esc.
    loop {
        let rows = provider_auth.0.login_options().await;
        let mut seen = std::collections::HashSet::new();
        let options: Vec<crate::onboarding_flow::ProviderPickerOption> = rows
            .iter()
            .filter(|row| !row.id.starts_with("mcp:") && seen.insert(row.id.clone()))
            .map(|row| crate::onboarding_flow::ProviderPickerOption {
                id: row.id.clone(),
                name: crate::menu_panel::scrub_controls(&row.name),
                connected: row.configured,
                available: row.available,
            })
            .collect();

        let screen = crate::onboarding::OnboardingScreen::welcome(options);
        let (screen, outcome) =
            drive_onboarding_pane(view, &mut *drive, screen, None, &mut session.osc_sink).await?;
        let pick = match outcome {
            PaneOutcome::InputClosed => return Ok(false),
            PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Exit) => return Ok(true),
            PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Pick(pick)) => pick,
            PaneOutcome::Decision(
                crate::onboarding::OnboardingDecision::Selected(_)
                | crate::onboarding::OnboardingDecision::Cancelled,
            )
            | PaneOutcome::Flow(_) => {
                unreachable!("the welcome picker yields Pick only")
            }
        };

        match pick {
            crate::onboarding_flow::ProviderPick::Continue => {
                // "Continue without signing in": blank initialization complete.
                if let Err(error) = task.sink.mark_onboarding_complete() {
                    warn_onboarding_persist_failure(session, view, &error);
                }
                return Ok(false);
            }
            crate::onboarding_flow::ProviderPick::Cancelled => {
                // Esc: skip for now; the marker stays unset and the welcome screen returns next launch.
                return Ok(false);
            }
            crate::onboarding_flow::ProviderPick::Provider(provider_id) => {
                let Some(row) = rows.iter().find(|r| r.id == provider_id).cloned() else {
                    continue;
                };
                let auth_ok =
                    run_provider_auth(session, view, drive, &provider_auth, screen, &row).await?;
                if !auth_ok {
                    continue;
                }

                // Auth succeeded: fetch the provider's models and show the model picker.
                let models = match session.fetch_models_for_provider(&provider_id).await {
                    Ok(models) => models,
                    Err(error) => {
                        warn_model_catalog_failure(session, view, &error);
                        continue;
                    }
                };
                if models.is_empty() {
                    // No models returned — treat as complete without a model pick.
                    if let Err(error) = task.sink.mark_onboarding_complete() {
                        warn_onboarding_persist_failure(session, view, &error);
                    }
                    return Ok(false);
                }
                let model_options: Vec<crate::onboarding_flow::ProviderPickerOption> = models
                    .iter()
                    .map(|model| crate::onboarding_flow::ProviderPickerOption {
                        id: model.id.clone(),
                        name: model.id.clone(),
                        connected: false,
                        available: true,
                    })
                    .collect();
                let picker = crate::onboarding_flow::ProviderPicker::model_picker(model_options);
                let screen = crate::onboarding::OnboardingScreen::with_panel(
                    crate::onboarding_flow::OnboardingPanel::Providers(picker),
                );
                let (_screen, outcome) =
                    drive_onboarding_pane(view, &mut *drive, screen, None, &mut session.osc_sink)
                        .await?;
                let model_pick = match outcome {
                    PaneOutcome::InputClosed => return Ok(false),
                    PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Exit) => {
                        return Ok(true)
                    }
                    PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Pick(pick)) => {
                        pick
                    }
                    PaneOutcome::Decision(
                        crate::onboarding::OnboardingDecision::Selected(_)
                        | crate::onboarding::OnboardingDecision::Cancelled,
                    )
                    | PaneOutcome::Flow(_) => {
                        unreachable!("the model picker yields Pick only")
                    }
                };
                match model_pick {
                    crate::onboarding_flow::ProviderPick::Continue
                    | crate::onboarding_flow::ProviderPick::Cancelled => {
                        // Back to the provider picker.
                    }
                    crate::onboarding_flow::ProviderPick::Provider(model_id) => {
                        session
                            .apply_model_selection(&provider_id, &model_id, view)
                            .await;
                        if let Err(error) = task.sink.mark_onboarding_complete() {
                            warn_onboarding_persist_failure(session, view, &error);
                        }
                        return Ok(false);
                    }
                }
            }
        }
    }
}

/// Run one provider's auth flow (API-key paste or terminal/OAuth) and return whether it succeeded.
/// On failure or cancel the caller loops back to the provider picker.
async fn run_provider_auth(
    session: &mut SessionUi,
    view: &mut AgentView,
    drive: &mut PaneDrive<'_>,
    provider_auth: &crate::provider_auth::ProviderAuthCommandsHandle,
    screen: crate::onboarding::OnboardingScreen,
    row: &crate::provider_auth::ProviderRow,
) -> Result<bool> {
    let provider_id = row.id.clone();
    let success = if row.flow == crate::provider_auth::AuthFlow::ApiKeyPrompt {
        let panel = session.auth_panel_handle();
        let prompt_cancel = panel.cancel_signal();
        let mut api_key_dialog =
            crate::auth_panel::AuthPanel::onboarding(format!("Login to {}", row.name));
        api_key_dialog.set_cancel_signal(prompt_cancel.clone());
        let mut screen = screen;
        screen.mount_panel(crate::onboarding_flow::OnboardingPanel::Auth {
            panel: std::boxed::Box::new(api_key_dialog),
            heading: None,
        });
        let prompt_cancel_body = prompt_cancel.clone();
        let row = row.clone();
        let prompt_auth = provider_auth.clone();
        let prompt_flow = OnboardingFlowTask::spawn(
            async move {
                match panel
                    .paste_prompt(
                        crate::onboarding_flow::API_KEY_PROMPT,
                        crate::auth_panel::PastePromptTone::Text,
                        crate::auth_panel::PasteStyle::Masked,
                    )
                    .await
                {
                    Some(api_key) if !prompt_cancel_body.cancelled() => {
                        prompt_auth.0.login(&row, Some(&api_key)).await
                    }
                    _ => crate::provider_auth::ProviderAuthOutcome::Cancelled,
                }
            },
            prompt_cancel,
        );
        let (_screen, outcome) = drive_onboarding_pane(
            view,
            &mut *drive,
            screen,
            Some(prompt_flow),
            &mut session.osc_sink,
        )
        .await?;
        match outcome {
            PaneOutcome::InputClosed => return Ok(false),
            PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Exit) => return Ok(true),
            PaneOutcome::Flow(result) => {
                let outcome = result.unwrap_or_else(|_| {
                    crate::provider_auth::ProviderAuthOutcome::Error(
                        "the provider login task failed".to_string(),
                    )
                });
                let ok = matches!(
                    outcome,
                    crate::provider_auth::ProviderAuthOutcome::Status(_)
                );
                session
                    .apply_auth_outcome(outcome, &provider_id, view)
                    .await;
                ok
            }
            PaneOutcome::Decision(_) => unreachable!("the key prompt dialog yields no decisions"),
        }
    } else {
        let panel = session.auth_panel_handle();
        let service_cancel = panel.cancel_signal();
        let mut service_dialog =
            crate::auth_panel::AuthPanel::onboarding(format!("Login to {}", row.name));
        service_dialog.set_cancel_signal(service_cancel.clone());
        let mut screen = screen;
        screen.mount_panel(crate::onboarding_flow::OnboardingPanel::Auth {
            panel: std::boxed::Box::new(service_dialog),
            heading: None,
        });
        let row = row.clone();
        let service_auth = provider_auth.clone();
        let provider_login = OnboardingFlowTask::spawn(
            async move { service_auth.0.login_on_panel(&row, panel).await },
            service_cancel,
        );
        let (_screen, outcome) = drive_onboarding_pane(
            view,
            &mut *drive,
            screen,
            Some(provider_login),
            &mut session.osc_sink,
        )
        .await?;
        match outcome {
            PaneOutcome::InputClosed => return Ok(false),
            PaneOutcome::Decision(crate::onboarding::OnboardingDecision::Exit) => return Ok(true),
            PaneOutcome::Flow(result) => {
                let outcome = result.unwrap_or_else(|_| {
                    crate::provider_auth::ProviderAuthOutcome::Error(
                        "the provider login task failed".to_string(),
                    )
                });
                let ok = matches!(
                    outcome,
                    crate::provider_auth::ProviderAuthOutcome::Status(_)
                );
                session
                    .apply_auth_outcome(outcome, &provider_id, view)
                    .await;
                ok
            }
            PaneOutcome::Decision(_) => unreachable!("the login dialog yields no decisions"),
        }
    };
    Ok(success)
}

/// The signed-in provider's model list could not be loaded: say so and return to the provider
/// picker, so the user can retry or continue without a model instead of finishing silently.
fn warn_model_catalog_failure(
    session: &mut SessionUi,
    view: &mut AgentView,
    error: &anyhow::Error,
) {
    view.push_entry(crate::chat::ChatEntry::Status {
        text: format!(
            "\u{26a0} The model list could not be loaded ({error}); pick a provider to try again."
        ),
        kind: crate::chat::StatusKind::Warning,
    });
    session.dirty = true;
}

/// A failed onboarding persistence write surfaces as a warning row in the session: the flow
/// still settled for this run, but an unpersisted marker re-mounts the whole flow on the next
/// launch — the user must know, and the run never dies over a settings write.
fn warn_onboarding_persist_failure(
    session: &mut SessionUi,
    view: &mut AgentView,
    error: &anyhow::Error,
) {
    view.push_entry(crate::chat::ChatEntry::Status {
        text: format!(
            "\u{26a0} The onboarding answer could not be saved ({error}); the first-run flow may appear again."
        ),
        kind: crate::chat::StatusKind::Warning,
    });
    session.dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_timing_marker_ends_active_login_flow() {
        let (auth_tx, mut auth_rx) = mpsc::unbounded_channel();
        let panel = crate::auth_panel::AuthPanelHandle::new(auth_tx);
        let cancel = panel.cancel_signal();
        let flow_cancel = cancel.clone();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let flow = OnboardingFlowTask::spawn(
            async move {
                while !flow_cancel.cancelled() {
                    tokio::task::yield_now().await;
                }
                done_tx.send(()).expect("flow completion receiver");
                crate::provider_auth::ProviderAuthOutcome::Cancelled
            },
            cancel.clone(),
        );
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (timing_tx, _timing_rx) = std::sync::mpsc::channel();
        ui_tx
            .send(UiInput::Timestamp(timing_tx))
            .expect("queued marker");
        let mut view = AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ));
        let mut renderer = Renderer::Headless {
            width: 80,
            height: 24,
            frames: Vec::new(),
            renders: 0,
        };
        let exit_guard = ExitGuard::new();
        let mut headless_done = false;
        let mut drive = PaneDrive {
            ui_rx: &mut ui_rx,
            renderer: &mut renderer,
            exit_guard: &exit_guard,
            keybindings: KeybindingsManager::new(),
            auth_panel_rx: &mut auth_rx,
            headless_done: &mut headless_done,
        };
        let mut osc_sink = crate::clipboard::OscSink::Buffer(Vec::new());
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            drive_onboarding_pane(
                &mut view,
                &mut drive,
                crate::onboarding::OnboardingScreen::default(),
                Some(flow),
                &mut osc_sink,
            ),
        )
        .await
        .expect("pane exits promptly")
        .err()
        .expect("onboarding rejects session timing markers");
        assert_eq!(
            error.to_string(),
            "headless timing markers require an attached session"
        );
        assert!(cancel.cancelled());
        assert_eq!(done_rx.try_recv(), Ok(()));
    }
}
