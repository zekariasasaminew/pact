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

pub use adapter::{
    adapter, resolve_safety_profile, AcpLaunchRequest, AgentAdapter, AgentKind, CoordConfig, LaunchRequest, LaunchSpec,
    SafetyProfile,
};
pub use event::AgentEvent;
pub use process::{run_and_stream, RunOutcome};
pub use supervisor::Supervisor;

/// What to spawn directly for `program`, without a shell: on Windows an
/// npm `.cmd` shim resolves to its `node.exe` plus script (see
/// `windows_shim`), a plain `.exe` to itself; elsewhere the name is used
/// as is. Returns the program and the arguments that must precede the
/// caller's own. For callers that drive the process themselves (the ACP
/// runtime, issue #331) rather than through `run_and_stream`.
pub fn resolve_program(program: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        if let Some(resolved) = windows_shim::resolve(program) {
            return (resolved.program.to_string_lossy().into_owned(), resolved.leading_args);
        }
    }
    (program.to_string(), Vec::new())
}
