//! Integration coverage for pact-acp (issue #330) against the
//! `fake_acp_agent` binary: real child process, real stdio JSON-RPC, no
//! model. The blocking `AcpRuntime` facade is what pact-core's lane
//! threads will call, so that is the surface under test.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pact_acp::{AcpError, AcpRuntime, AgentSpec, McpServer, PermissionDecision, StopReason};
use uuid::Uuid;

fn fake_spec(extra_env: Vec<(String, String)>) -> AgentSpec {
    AgentSpec { program: env!("CARGO_BIN_EXE_fake_acp_agent").to_string(), args: Vec::new(), env: extra_env, cwd: None }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-acp-{tag}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn task(writes: &[(&str, &str)], summary: &str) -> String {
    serde_json::json!({
        "writes": writes.iter().cloned().collect::<std::collections::BTreeMap<&str, &str>>(),
        "summary": summary,
    })
    .to_string()
}

fn read(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file)).ok()
}

#[test]
fn initialize_reports_the_agent_and_its_capabilities() {
    let runtime = AcpRuntime::start_unattended(fake_spec(Vec::new())).unwrap();
    let init = runtime.initialize_result();
    assert_eq!(init.protocol_version, 1);
    assert_eq!(init.agent_info.as_ref().map(|a| a.name.as_str()), Some("fake-acp-agent"));
    assert!(init.agent_capabilities.pointer("/sessionCapabilities/close").is_some());
    assert!(runtime.pid() > 0);
    runtime.shutdown();
}

/// The property the whole runtime rests on: lanes are sessions in one
/// process, each writing into its own directory, prompted concurrently
/// from separate threads, each seeing only its own updates.
#[test]
fn two_sessions_prompted_concurrently_each_write_into_their_own_cwd() {
    let runtime = Arc::new(AcpRuntime::start_unattended(fake_spec(Vec::new())).unwrap());
    let dir_a = temp_dir("lane-a");
    let dir_b = temp_dir("lane-b");
    let session_a = runtime.new_session(&dir_a, vec![McpServer::http("pact-coord", "http://127.0.0.1:1/lanes/a")]).unwrap();
    let session_b = runtime.new_session(&dir_b, Vec::new()).unwrap();
    assert_ne!(session_a.id, session_b.id);

    let run = |mut session: pact_acp::LaneSession, dir: PathBuf, file: &'static str, content: &'static str, summary: &'static str| {
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            let own_id = session.id.clone();
            let mut text = String::new();
            let mut tools = Vec::new();
            let mut foreign = 0;
            let stop = runtime
                .prompt(&mut session, &task(&[(file, content)], summary), |update| {
                    if update.session_id != own_id {
                        foreign += 1;
                    }
                    if let Some(t) = update.text() {
                        text.push_str(t);
                    }
                    if let Some(title) = update.tool_title() {
                        tools.push(title.to_string());
                    }
                })
                .unwrap();
            (stop, text, tools, foreign, dir, session)
        })
    };
    let a = run(session_a, dir_a.clone(), "alpha.txt", "ALPHA", "wrote alpha");
    let b = run(session_b, dir_b.clone(), "beta.txt", "BETA", "wrote beta");
    let (stop_a, text_a, tools_a, foreign_a, _, session_a) = a.join().unwrap();
    let (stop_b, text_b, tools_b, foreign_b, _, session_b) = b.join().unwrap();

    assert_eq!(stop_a, StopReason::EndTurn);
    assert_eq!(stop_b, StopReason::EndTurn);
    assert_eq!(text_a, "wrote alpha", "chunks arrive in order and only for this session");
    assert_eq!(text_b, "wrote beta");
    assert_eq!(tools_a, vec!["Creating alpha.txt"]);
    assert_eq!(tools_b, vec!["Creating beta.txt"]);
    assert_eq!(foreign_a + foreign_b, 0, "an update must never reach another lane's callback");
    assert_eq!(read(&dir_a, "alpha.txt").as_deref(), Some("ALPHA"));
    assert_eq!(read(&dir_b, "beta.txt").as_deref(), Some("BETA"));
    assert!(read(&dir_a, "beta.txt").is_none() && read(&dir_b, "alpha.txt").is_none(), "each session writes into its own cwd");

    runtime.close(&session_a).unwrap();
    runtime.close(&session_b).unwrap();
    Arc::try_unwrap(runtime).ok().expect("no other handles").shutdown();
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn permission_requests_are_answered_by_the_policy() {
    // Default policy allows: the write happens.
    let runtime = AcpRuntime::start_unattended(fake_spec(Vec::new())).unwrap();
    let dir = temp_dir("perm-allow");
    let mut session = runtime.new_session(&dir, Vec::new()).unwrap();
    let text = serde_json::json!({ "writes": { "guarded.txt": "ok" }, "summary": "asked first", "ask_permission": true }).to_string();
    let stop = runtime.prompt(&mut session, &text, |_| {}).unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(read(&dir, "guarded.txt").as_deref(), Some("ok"), "the default policy must allow the tool call");
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);

    // A rejecting policy: the agent skips the write and the turn still ends cleanly.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let policy: pact_acp::PermissionPolicy = Arc::new(move |request| {
        recorder.lock().unwrap().push(request.options.iter().map(|o| o.option_id.clone()).collect::<Vec<_>>());
        PermissionDecision::Select(request.options.iter().find(|o| !o.kind.allows()).unwrap().option_id.clone())
    });
    let runtime = AcpRuntime::start(fake_spec(Vec::new()), policy).unwrap();
    let dir = temp_dir("perm-reject");
    let mut session = runtime.new_session(&dir, Vec::new()).unwrap();
    let stop = runtime.prompt(&mut session, &text, |_| {}).unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert!(read(&dir, "guarded.txt").is_none(), "a rejected permission must not write");
    assert_eq!(seen.lock().unwrap().as_slice(), &[vec!["allow".to_string(), "reject".to_string()]]);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_agent_that_dies_mid_turn_fails_the_pending_prompt_and_every_later_call() {
    let runtime = AcpRuntime::start_unattended(fake_spec(Vec::new())).unwrap();
    let dir = temp_dir("exit");
    let mut session = runtime.new_session(&dir, Vec::new()).unwrap();
    let text = serde_json::json!({ "writes": { "never.txt": "x" }, "summary": "dies", "exit_mid_turn": true }).to_string();
    let mut saw_tool_call = false;
    let err = runtime.prompt(&mut session, &text, |u| saw_tool_call |= u.kind == "tool_call").unwrap_err();
    assert!(matches!(err, AcpError::RuntimeExited { .. }), "got: {err}");
    assert!(saw_tool_call, "updates sent before the crash must still be delivered");
    assert!(read(&dir, "never.txt").is_none());
    assert!(runtime.exit_info().is_some(), "the exit must be recorded on the runtime");

    let later = runtime.new_session(&dir, Vec::new()).unwrap_err();
    assert!(matches!(later, AcpError::RuntimeExited { .. }), "later calls fail fast: {later}");
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_and_close_reach_the_agent_and_an_unknown_session_is_an_rpc_error() {
    let log = std::env::temp_dir().join(format!("pact-acp-log-{}.txt", Uuid::new_v4()));
    let runtime = AcpRuntime::start_unattended(fake_spec(vec![("FAKE_ACP_LOG".into(), log.to_string_lossy().to_string())])).unwrap();
    let dir = temp_dir("cancel");
    let mut session = runtime.new_session(&dir, Vec::new()).unwrap();
    runtime.cancel(&session).unwrap();
    runtime.close(&session).unwrap();

    // The fake forgets a closed session, so prompting it again is the
    // agent's own -32602, surfaced as an Rpc error rather than a hang.
    let err = runtime.prompt(&mut session, "{}", |_| {}).unwrap_err();
    assert!(matches!(err, AcpError::Rpc { code: -32602, .. }), "got: {err}");

    runtime.shutdown();
    let methods = std::fs::read_to_string(&log).unwrap();
    for expected in ["initialize", "session/new", "session/cancel", "session/close", "session/prompt"] {
        assert!(methods.lines().any(|l| l == expected), "agent never saw {expected}; saw:\n{methods}");
    }
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&dir);
}
