//! Headless e2e for the busy state across a turn's end: `turn_end` settles the
//! turn's cards but the loader keeps spinning until `agent_end` (the run's end),
//! and the bash card held during the turn flushes into the transcript only then.
//! The regression: `turn_end` cleared the loader and flushed the held card one
//! run-tail early, while the agent still held the run open.
#![cfg(unix)]

mod support;

use std::thread;
use std::time::Duration;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, UiMode,
};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, serve_connection,
    write_json, write_session_event, MockSupervisor, Request,
};

/// Milliseconds (from attach) at which the mock's events land.
const BASH_START_MS: u64 = 200;
const BASH_END_MS: u64 = 400;
const TURN_END_MS: u64 = 1000;
const AGENT_END_MS: u64 = 3000;

impl MockSupervisor {
    /// Serve one client connection: attach to a streaming turn, then run a bash
    /// command mid-turn, end the turn, and end the run later.
    fn serve(self) {
        let Some(stream) = accept_within_idle_window(&self.listener) else {
            return;
        };
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &attach_data(id));
                    let mut event_writer = writer.try_clone().expect("clone event socket");
                    thread::spawn(move || emit_turn(&mut event_writer));
                }
                _ => return false,
            }
            true
        });
    }
}

/// The attach result: a streaming snapshot whose prompt landed just now.
fn attach_data(id: &str) -> Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis();
    let mut response = attach_response(id, "busy state session");
    response["data"]["snapshot"]["state"]["isStreaming"] = json!(true);
    response["data"]["snapshot"]["messages"] = json!([
        {
            "role": "user",
            "content": [{ "type": "text", "text": "run the sweep" }],
            "timestamp": now_ms,
        },
    ]);
    response
}

/// The turn's event schedule, paced from attach. Each step waits for the
/// previous one's delay, so the frames arrive in this order.
fn emit_turn(writer: &mut std::os::unix::net::UnixStream) {
    let schedule: [(u64, Value); 4] = [
        (
            BASH_START_MS,
            json!({ "type": "bash_start", "command": "echo held" }),
        ),
        (
            BASH_END_MS,
            json!({ "type": "bash_end", "exitCode": 0, "cancelled": false }),
        ),
        (TURN_END_MS, json!({ "type": "turn_end" })),
        (AGENT_END_MS, json!({ "type": "agent_end" })),
    ];
    let mut elapsed = 0;
    for (at, event) in schedule {
        thread::sleep(Duration::from_millis(at - elapsed));
        elapsed = at;
        write_session_event(writer, &event);
    }
}

#[test]
fn the_busy_state_holds_past_turn_end_until_the_run_ends() {
    // The ambient TMUX variable adds a startup notice to the transcript.
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
    let handle = thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitMs(AGENT_END_MS + 1000)],
        width: 100,
        height: 40,
    };
    let outcome = runtime
        .block_on(run_interactive(
            InteractiveOptions {
                fullscreen_mouse: false,
                ..options(socket)
            },
            UiMode::Headless(plan),
        ))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");

    let loader_lines: Vec<&str> = outcome
        .frames
        .iter()
        .filter_map(|frame| frame.lines().find(|line| line.contains("Waiting")))
        .collect();
    let elapsed_secs: Vec<u64> = loader_lines
        .iter()
        .filter_map(|line| {
            line.split('\u{b7}')
                .nth(1)?
                .trim()
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .collect();
    let transcript = loader_lines.join("\n");
    // The loader runs past `turn_end` (at ~1s): a counter past 2s can only
    // paint if the busy state survived the turn's end.
    assert!(
        elapsed_secs.iter().any(|secs| *secs >= 2),
        "the loader keeps running after turn_end until agent_end:\n{transcript}"
    );
    let last = outcome.frames.last().expect("a final frame");
    assert!(
        !last.contains("Waiting"),
        "the loader clears once the run ends:\n{last}"
    );
    assert_eq!(
        last.matches("$ echo held").count(),
        1,
        "the held bash card flushes into the transcript at agent_end:\n{last}"
    );
}
