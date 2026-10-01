//! A fake ACP agent for tests: speaks just enough of the Agent Client
//! Protocol over stdio to exercise the client and the lane runtime, never
//! calls a model. Shipped as a library module so both pact-acp's own test
//! binary and pact-cli's (which must impersonate `copilot` on PATH) are
//! one-line wrappers around [`main`].
//!
//! Each `session/prompt`'s text is a JSON task:
//!
//! ```json
//! {"writes": {"a.txt": "A"}, "summary": "did a", "ask_permission": false,
//!  "exit_mid_turn": false, "stop": "end_turn", "sleep_ms": 0}
//! ```
//!
//! Behaviour per prompt: emit a `usage_update`; if `ask_permission`, send
//! `session/request_permission` and wait for the answer (a rejection or
//! cancellation skips the writes); for each write emit `tool_call`, write
//! the file into the session's cwd, emit `tool_call_update`; if
//! `exit_mid_turn`, exit with status 3 after the first tool call; emit the
//! summary as two `agent_message_chunk`s; answer with `stop`. Every method
//! received is appended to the file named by `FAKE_ACP_LOG`, if set.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};

use serde_json::{json, Value};

struct Fake {
    out: std::io::Stdout,
    sessions: HashMap<String, String>,
    next_session: usize,
    next_request: usize,
    backlog: VecDeque<Value>,
    log: Option<std::path::PathBuf>,
}

impl Fake {
    fn send(&mut self, msg: Value) {
        let mut out = self.out.lock();
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    fn log(&self, method: &str) {
        if let Some(path) = &self.log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{method}");
            }
        }
    }

    fn update(&mut self, session_id: &str, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session_id, "update": update } }));
    }

    /// Reads stdin until the response to `id` arrives, buffering anything
    /// else to handle after the current turn.
    fn await_response(&mut self, lines: &mut impl Iterator<Item = String>, id: &Value) -> Option<Value> {
        for line in lines {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("method").is_none() && msg.get("id") == Some(id) {
                return Some(msg);
            }
            self.backlog.push_back(msg);
        }
        None
    }

    fn handle(&mut self, msg: Value, lines: &mut impl Iterator<Item = String>) {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if !method.is_empty() {
            self.log(&method);
        }
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method.as_str(), id) {
            ("initialize", Some(id)) => self.send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": false, "sessionCapabilities": { "close": {} } },
                    "agentInfo": { "name": "fake-acp-agent", "version": "0.0.0" },
                    "authMethods": [],
                },
            })),
            ("session/new", Some(id)) => {
                let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or(".").to_string();
                self.next_session += 1;
                let session_id = format!("sess-{}", self.next_session);
                self.sessions.insert(session_id.clone(), cwd);
                let mcp_names: Vec<Value> = params
                    .get("mcpServers")
                    .and_then(Value::as_array)
                    .map(|servers| servers.iter().filter_map(|s| s.get("name").cloned()).collect())
                    .unwrap_or_default();
                self.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "sessionId": session_id, "configOptions": [], "_meta": { "mcpServers": mcp_names } },
                }));
            }
            ("session/prompt", Some(id)) => self.prompt(id, params, lines),
            ("session/close", Some(id)) => {
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                    self.sessions.remove(session_id);
                }
                self.send(json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
            }
            ("session/cancel", None) => {}
            (_, Some(id)) => self.send(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("fake agent does not implement {method}") },
            })),
            (_, None) => {}
        }
    }

    fn prompt(&mut self, id: Value, params: Value, lines: &mut impl Iterator<Item = String>) {
        let session_id = params.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(cwd) = self.sessions.get(&session_id).cloned() else {
            self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": format!("unknown session {session_id}") } }));
            return;
        };
        let text = params
            .pointer("/prompt/0/text")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string();
        let task = parse_task(&text);

        self.update(&session_id, json!({ "sessionUpdate": "usage_update", "used": 10, "size": 1000 }));

        if let Some(ms) = task.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }

        let mut allowed = true;
        if task.get("ask_permission").and_then(Value::as_bool).unwrap_or(false) {
            self.next_request += 1;
            let request_id = json!(format!("perm-{}", self.next_request));
            self.send(json!({
                "jsonrpc": "2.0", "id": request_id, "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "toolCall": { "toolCallId": "call-1", "title": "Write files" },
                    "options": [
                        { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                    ],
                },
            }));
            allowed = match self.await_response(lines, &request_id) {
                Some(reply) => reply.pointer("/result/outcome/optionId").and_then(Value::as_str) == Some("allow"),
                None => false,
            };
        }

        if allowed {
            let writes = task.get("writes").and_then(Value::as_object).cloned().unwrap_or_default();
            for (index, (file, content)) in writes.iter().enumerate() {
                let call_id = format!("call-{}", index + 1);
                self.update(&session_id, json!({ "sessionUpdate": "tool_call", "toolCallId": call_id, "title": format!("Creating {file}"), "kind": "edit", "status": "pending" }));
                if task.get("exit_mid_turn").and_then(Value::as_bool).unwrap_or(false) {
                    std::process::exit(3);
                }
                let path = std::path::Path::new(&cwd).join(file);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, content.as_str().unwrap_or("")).expect("fake agent write");
                self.update(&session_id, json!({ "sessionUpdate": "tool_call_update", "toolCallId": call_id, "status": "completed" }));
            }
        }

        let summary = task.get("summary").and_then(Value::as_str).unwrap_or("done").to_string();
        let mid = summary.char_indices().nth(summary.chars().count() / 2).map(|(i, _)| i).unwrap_or(0);
        let (head, tail) = summary.split_at(mid);
        for piece in [head, tail] {
            self.update(&session_id, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": piece } }));
        }
        let stop = if allowed { task.get("stop").and_then(Value::as_str).unwrap_or("end_turn").to_string() } else { "end_turn".to_string() };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": stop } }));
    }
}

/// The JSON task inside a prompt. pact may prepend prose to the task (the
/// shared-tree preamble, issue #315), so the first `{` to the last `}` is
/// tried when the whole text is not JSON; anything else is a plain prompt
/// with nothing to write.
fn parse_task(text: &str) -> Value {
    if let Ok(task) = serde_json::from_str::<Value>(text) {
        return task;
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if end > start {
            if let Ok(task) = serde_json::from_str::<Value>(&text[start..=end]) {
                return task;
            }
        }
    }
    json!({ "summary": text })
}

/// Runs the fake agent on this process's stdin/stdout until EOF.
pub fn main() {
    let stdin = std::io::stdin();
    let mut fake = Fake {
        out: std::io::stdout(),
        sessions: HashMap::new(),
        next_session: 0,
        next_request: 0,
        backlog: VecDeque::new(),
        log: std::env::var_os("FAKE_ACP_LOG").map(std::path::PathBuf::from),
    };
    let mut lines = stdin.lock().lines().map_while(Result::ok);
    loop {
        while let Some(msg) = fake.backlog.pop_front() {
            fake.handle(msg, &mut lines);
        }
        let Some(line) = lines.next() else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(msg) => fake.handle(msg, &mut lines),
            Err(_) => continue,
        }
    }
}
