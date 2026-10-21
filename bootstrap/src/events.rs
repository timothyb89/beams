use std::io::{self, IsTerminal, Write};

use serde::Serialize;

/// Lifecycle status of a bootstrap step.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Start,
    Ok,
    Skip,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Start => "..",
            Status::Ok => "ok",
            Status::Skip => "skip",
            Status::Error => "error",
        }
    }
}

/// A single progress event for one step. Steps emit `Start`, then exactly
/// one of `Ok` / `Skip` / `Error` once they're done. This is the schema the
/// future client wrapper will consume over SSH exec in `--json` mode.
#[derive(Serialize)]
pub struct Event {
    pub r#type: &'static str,
    pub step: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Event {
    pub fn new(step: &str, status: Status, message: Option<String>) -> Self {
        Event {
            r#type: "step",
            step: step.to_string(),
            status,
            message,
        }
    }
}

/// How to render emitted events. `&self`, not `&mut self`: both impls
/// below are stateless (just write to stdout, which locks internally
/// around each `println!` call), and `steps::run` now calls `emit` from
/// several threads at once when a wave has more than one task in it -
/// see `executor::run`.
pub trait Emitter: Sync {
    fn emit(&self, event: Event);
}

/// Raw JSON-lines output, one `Event` per line on stdout. This is the format
/// a client wrapper would pipe and parse.
pub struct Json;

impl Emitter for Json {
    fn emit(&self, event: Event) {
        // Each step's lifecycle is a handful of events; losing one to a
        // serialization bug would be a real bug, so unwrap rather than
        // silently dropping output.
        println!("{}", serde_json::to_string(&event).unwrap());
        let _ = io::stdout().flush();
    }
}

/// Human-readable rendering, used by default when stdout is a TTY.
pub struct Pretty;

impl Emitter for Pretty {
    fn emit(&self, event: Event) {
        match event.status {
            Status::Start => println!("[{}] running...", event.step),
            _ => {
                let label = event.status.label();
                match event.message {
                    Some(message) => println!("[{}] {label}: {message}", event.step),
                    None => println!("[{}] {label}", event.step),
                }
            }
        }
    }
}

/// Pick `Json` when explicitly requested or when stdout isn't a TTY
/// (future client piping), otherwise `Pretty`.
pub fn default_emitter(force_json: bool) -> Box<dyn Emitter> {
    if force_json || !io::stdout().is_terminal() {
        Box::new(Json)
    } else {
        Box::new(Pretty)
    }
}
