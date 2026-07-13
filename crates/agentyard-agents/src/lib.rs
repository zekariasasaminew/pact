//! Agent CLI adapters (Phase 2: Claude Code; Phase 4: Codex, Copilot CLI).
//!
//! Each adapter's job is just building the headless launch command for its
//! CLI (see `claude_code`); actually running it and normalizing its
//! streamed output is shared, adapter-agnostic machinery (`process`,
//! `event`), so adding Codex/Copilot in Phase 4 means one new small
//! `build_command`-shaped module, not touching process supervision.

pub mod claude_code;
mod event;
mod process;

pub use event::AgentEvent;
pub use process::{run_and_stream, RunOutcome};
