//! `prime-agent memory`: the page-store operations the consolidation pass and
//! operators share.
//!
//! Every subcommand is one `rlm.harness.cli_dispatch` payload through the kernel
//! Python. The kernel owns the page rules (what may be removed, what a merge
//! carries, when a pass records itself), so the CLI never re-implements a
//! decision — the two sides cannot drift.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// The thin runner executed by the kernel Python: one JSON payload in on stdin,
/// one JSON result out on stdout (`rlm.harness.cli_dispatch`).
const CLI_DISPATCH_RUNNER: &str = concat!(
    "import json, sys\n",
    "from rlm.harness import cli_dispatch\n",
    "payload = json.loads(sys.stdin.read() or \"{}\")\n",
    "print(json.dumps(cli_dispatch(payload)))\n",
);

/// The parsed `prime-agent memory` invocation.
#[derive(Debug, PartialEq, Eq)]
enum MemoryCommand {
    Status,
    Due,
    Plan,
    Merge {
        keep: String,
        absorb: String,
    },
    Promote {
        slug: String,
        to: Option<String>,
    },
    Delete {
        slug: String,
    },
    RecordPass {
        visited: u64,
        merged: u64,
        promoted: u64,
        removed: u64,
        compressed: u64,
        next_hours: u64,
        why: String,
    },
}

/// Parse the subcommand and its flags; `Err` carries a usage hint.
fn parse_memory_command(args: &[String]) -> Result<MemoryCommand, String> {
    let children: Vec<&str> = crate::command_registry::get_child_command_specs(&["memory"])
        .into_iter()
        .map(|spec| spec.path[spec.path.len() - 1])
        .collect();
    let Some(subcommand) = args.first() else {
        return Err(String::new());
    };
    if !children.contains(&subcommand.as_str()) {
        let suggestion = crate::command_registry::find_command_suggestion(subcommand, &children);
        return Err(format!(
            "Unknown memory command: {subcommand}{}",
            suggestion.map_or_else(String::new, |s| format!(" (did you mean \"{s}\"?)"))
        ));
    }
    let mut keep = None;
    let mut absorb = None;
    let mut to = None;
    let mut visited = 0_u64;
    let mut merged = 0_u64;
    let mut promoted = 0_u64;
    let mut removed = 0_u64;
    let mut compressed = 0_u64;
    let mut next_hours = 0_u64;
    let mut why = String::new();
    let mut operands: Vec<String> = Vec::new();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let mut value = |name: &str| -> Result<String, String> {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--keep" => keep = Some(value("--keep")?),
            "--absorb" => absorb = Some(value("--absorb")?),
            "--to" => to = Some(value("--to")?),
            "--visited" => {
                visited = value("--visited")?
                    .parse()
                    .map_err(|_| "--visited takes a number".to_string())?;
            }
            "--merged" => {
                merged = value("--merged")?
                    .parse()
                    .map_err(|_| "--merged takes a number".to_string())?;
            }
            "--promoted" => {
                promoted = value("--promoted")?
                    .parse()
                    .map_err(|_| "--promoted takes a number".to_string())?;
            }
            "--removed" => {
                removed = value("--removed")?
                    .parse()
                    .map_err(|_| "--removed takes a number".to_string())?;
            }
            "--compressed" => {
                compressed = value("--compressed")?
                    .parse()
                    .map_err(|_| "--compressed takes a number".to_string())?;
            }
            "--next-hours" => {
                next_hours = value("--next-hours")?
                    .parse()
                    .map_err(|_| "--next-hours takes a number".to_string())?;
            }
            "--why" => why = value("--why")?,
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => operands.push(other.to_string()),
        }
    }
    match subcommand.as_str() {
        "status" | "due" | "plan" if operands.is_empty() => Ok(match subcommand.as_str() {
            "status" => MemoryCommand::Status,
            "due" => MemoryCommand::Due,
            _ => MemoryCommand::Plan,
        }),
        "merge" => {
            let (Some(keep), Some(absorb)) = (keep, absorb) else {
                return Err(
                    "Usage: prime-agent memory merge --keep <slug> --absorb <slug>".to_string(),
                );
            };
            Ok(MemoryCommand::Merge { keep, absorb })
        }
        "promote" => match operands.as_slice() {
            [slug] => Ok(MemoryCommand::Promote {
                slug: slug.clone(),
                to,
            }),
            _ => Err("Usage: prime-agent memory promote <slug> [--to <type>]".to_string()),
        },
        "delete" => match operands.as_slice() {
            [slug] => Ok(MemoryCommand::Delete { slug: slug.clone() }),
            _ => Err("Usage: prime-agent memory delete <slug>".to_string()),
        },
        "record-pass" => Ok(MemoryCommand::RecordPass {
            visited,
            merged,
            promoted,
            removed,
            compressed,
            next_hours: if next_hours == 0 { 12 } else { next_hours },
            why,
        }),
        _ => Err(String::new()),
    }
}

/// Run the `memory` command; returns the process exit code.
pub fn run_memory_command(args: &[String]) -> i32 {
    let command = match parse_memory_command(args) {
        Ok(command) => command,
        Err(empty_hint) if empty_hint.is_empty() => {
            eprintln!("Missing memory command.");
            eprintln!("Run \"prime-agent help memory\" for usage.");
            return 1;
        }
        Err(error) => {
            eprintln!("Error: {error}");
            eprintln!("Run `prime-agent help memory` for usage.");
            return 1;
        }
    };
    let payload = match command {
        MemoryCommand::Status => json!({"op": "status"}),
        MemoryCommand::Due => json!({"op": "due"}),
        MemoryCommand::Plan => json!({"op": "plan"}),
        MemoryCommand::Merge { keep, absorb } => {
            json!({"op": "merge", "keep": keep, "absorb": absorb})
        }
        MemoryCommand::Promote { slug, to } => json!({"op": "promote", "slug": slug, "to": to}),
        MemoryCommand::Delete { slug } => json!({"op": "delete", "slug": slug}),
        MemoryCommand::RecordPass {
            visited,
            merged,
            promoted,
            removed,
            compressed,
            next_hours,
            why,
        } => json!({
            "op": "record-pass",
            "visited": visited,
            "merged": merged,
            "promoted": promoted,
            "removed": removed,
            "compressed": compressed,
            "next_hours": next_hours,
            "why": why,
        }),
    };
    match resolve_kernel_python().and_then(|python| dispatch_via_kernel(&python, &payload)) {
        Ok(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
            if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                0
            } else {
                for error in result
                    .get("errors")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    eprintln!("Error: {error}");
                }
                1
            }
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn resolve_kernel_python() -> Result<PathBuf, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start the CLI runtime: {error}"))?;
    runtime
        .block_on(pa_core::kernel::ensure_kernel_python(
            pa_core::kernel::EnsureKernelPythonOptions::default(),
        ))
        .map_err(|error| format!("kernel python unavailable: {error:#}"))
}

/// Drive one `rlm.harness.cli_dispatch` payload through the kernel Python.
fn dispatch_via_kernel(python: &std::path::Path, payload: &Value) -> Result<Value, String> {
    let mut child = Command::new(python);
    child
        .args(["-P", "-s", "-c", CLI_DISPATCH_RUNNER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(pa_core::kernel::python_process_environment());
    pa_core::platform::process::set_no_window(&mut child);
    let mut child = child
        .spawn()
        .map_err(|error| format!("failed to run the kernel python: {error}"))?;
    {
        let stdin = child.stdin.as_mut().expect("stdin was piped on this child");
        stdin
            .write_all(payload.to_string().as_bytes())
            .map_err(|error| format!("failed to send the payload: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to wait for the kernel python: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("the kernel python failed: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the kernel returned unusable JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_the_inspection_subcommands() {
        assert_eq!(
            parse_memory_command(&args(&["status"])),
            Ok(MemoryCommand::Status)
        );
        assert_eq!(
            parse_memory_command(&args(&["due"])),
            Ok(MemoryCommand::Due)
        );
        assert_eq!(
            parse_memory_command(&args(&["plan"])),
            Ok(MemoryCommand::Plan)
        );
    }

    #[test]
    fn parses_the_action_subcommands() {
        assert_eq!(
            parse_memory_command(&args(&["merge", "--keep", "a", "--absorb", "b"])),
            Ok(MemoryCommand::Merge {
                keep: "a".to_string(),
                absorb: "b".to_string()
            })
        );
        assert_eq!(
            parse_memory_command(&args(&["promote", "slug", "--to", "model"])),
            Ok(MemoryCommand::Promote {
                slug: "slug".to_string(),
                to: Some("model".to_string())
            })
        );
        assert_eq!(
            parse_memory_command(&args(&["delete", "slug"])),
            Ok(MemoryCommand::Delete {
                slug: "slug".to_string()
            })
        );
        let record =
            parse_memory_command(&args(&["record-pass", "--visited", "7", "--why", "steady"]))
                .expect("record-pass parses");
        assert_eq!(
            record,
            MemoryCommand::RecordPass {
                visited: 7,
                merged: 0,
                promoted: 0,
                removed: 0,
                compressed: 0,
                next_hours: 12,
                why: "steady".to_string(),
            }
        );
    }

    #[test]
    fn a_missing_operand_names_the_usage() {
        let error = parse_memory_command(&args(&["merge", "--keep", "a"])).unwrap_err();
        assert!(error.contains("memory merge --keep"), "{error}");
        let error = parse_memory_command(&args(&["promote"])).unwrap_err();
        assert!(error.contains("memory promote"), "{error}");
        let error = parse_memory_command(&args(&["delete"])).unwrap_err();
        assert!(error.contains("memory delete"), "{error}");
    }
}
