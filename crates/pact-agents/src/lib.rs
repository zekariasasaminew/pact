//! Agent CLI adapters.
//!
//! Each adapter's job is building the headless launch command for its CLI
//! and parsing its output into the shared `AgentEvent` model (see
//! `adapter::AgentAdapter`); actually running the process and driving that
//! parser is adapter-agnostic machinery (`process::run_and_stream`), so
//! adding an adapter means one new small module, not touching process
//! supervision. Claude Code and Copilot CLI are both live-verified; Codex
//! is implemented from documentation only -- see `codex.rs`'s doc comment.

mod adapter;
mod agy;
mod claude_code;
mod codex;
mod copilot;
mod event;
mod gemini;
mod process;
mod supervisor;
#[cfg(windows)]
mod windows_shim;

pub use adapter::{adapter, resolve_safety_profile, AgentAdapter, AgentKind, CoordConfig, SafetyProfile};
pub use event::AgentEvent;
pub use process::{run_and_stream, RunOutcome};
pub use supervisor::Supervisor;
