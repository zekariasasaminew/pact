//! End-to-end coverage for `pact ui` (issue #390): the real `pact`
//! binary serving on a free port, a real git repository, `fake_agent` on
//! `PATH` as `claude`, and plain HTTP over a `TcpStream`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-ui-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(name: &str) -> PathBuf {
    let root = scratch_dir(name);
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn shim_dir() -> PathBuf {
    let dir = scratch_dir("shim");
    let dest = if cfg!(windows) { dir.join("claude.exe") } else { dir.join("claude") };
    std::fs::copy(env!("CARGO_BIN_EXE_fake_agent"), &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A running `pact ui`, killed when the test ends however it ends.
struct Server {
    child: Child,
    /// Held open so the lines `pact ui` prints after the first do not
    /// hit a closed pipe.
    _stdout: BufReader<std::process::ChildStdout>,
    host: String,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|err| panic!("{err}: {} {}", self.status, self.body))
    }
}

fn request(address: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Reply {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut text = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        text.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    stream.write_all(format!("{text}\r\n{body}").as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or_else(|| panic!("not an HTTP reply: {raw}"));
    let status = head.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or_else(|| panic!("no status: {head}"));
    Reply { status, head: head.to_ascii_lowercase(), body: body.to_string() }
}

fn query_escape(text: &str) -> String {
    text.bytes()
        .map(|byte| if byte.is_ascii_alphanumeric() { (byte as char).to_string() } else { format!("%{byte:02X}") })
        .collect()
}

impl Server {
    fn start(cwd: &Path, shim: &Path, state: &Path) -> Self {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = format!("{}{sep}{}", shim.display(), std::env::var("PATH").unwrap_or_default());
        let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["ui", "--no-open", "--json", "--port", "0", "--stagger-ms", "0", "--min-free-mem-mb", "0"])
            .current_dir(cwd)
            .env("PATH", path)
            .env("PACT_UI_STATE_DIR", state)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut first = String::new();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        stdout.read_line(&mut first).unwrap();
        let mut server = Self { child, _stdout: stdout, host: String::new(), token: String::new() };
        let greeting: Value = serde_json::from_str(&first).unwrap_or_else(|err| panic!("{err}: unexpected first line: {first}"));
        server.host = greeting["url"]
            .as_str()
            .and_then(|url| url.strip_prefix("http://"))
            .unwrap_or_else(|| panic!("no address in the first line: {first}"))
            .trim_end_matches('/')
            .to_string();

        let page = request(&server.host, "GET", "/", &[("Host", &server.host)], None);
        assert_eq!(page.status, 200, "{}", page.body);
        assert!(page.head.contains("content-security-policy: default-src 'none'"), "{}", page.head);
        assert!(page.head.contains("x-frame-options: deny"), "{}", page.head);
        server.token = page
            .body
            .split_once("const TOKEN = \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(token, _)| token.to_string())
            .expect("the page carries its token");
        assert_eq!(server.token.len(), 64, "{}", server.token);
        assert_eq!(greeting["token"], server.token.as_str(), "--json prints the token the page carries");
        server
    }

    fn get(&self, path: &str) -> Reply {
        request(&self.host, "GET", path, &[("Host", &self.host), ("X-Pact-Token", &self.token)], None)
    }

    fn post(&self, path: &str, body: &Value) -> Reply {
        request(&self.host, "POST", path, &[("Host", &self.host), ("X-Pact-Token", &self.token)], Some(body))
    }

    fn tasks(&self) -> Vec<Value> {
        let reply = self.get("/api/state");
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()["tasks"].as_array().cloned().unwrap()
    }

    fn wait_for_status(&self, id: &str, wanted: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let tasks = self.tasks();
            if let Some(task) = tasks.iter().find(|task| task["id"] == id && task["status"] == wanted) {
                return task.clone();
            }
            assert!(Instant::now() < deadline, "task {id} never became {wanted}: {tasks:?}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

const DONE_REPORT: &str = "\nSTATUS: done\nCHANGED: two files\nVERIFIED: read them back\nNOT VERIFIED: nothing\nNOTES: none";

#[test]
fn ui_refuses_strangers_then_starts_reviews_and_lands_a_task() {
    let repo = init_repo("repo");
    let shim = shim_dir();
    let outside = scratch_dir("outside");
    let server = Server::start(&outside, &shim, &outside);

    let no_token = request(&server.host, "GET", "/api/state", &[("Host", &server.host)], None);
    assert_eq!(no_token.status, 403, "{}", no_token.body);
    let wrong_token = request(&server.host, "GET", "/api/state", &[("Host", &server.host), ("X-Pact-Token", "nope")], None);
    assert_eq!(wrong_token.status, 403, "{}", wrong_token.body);
    let rebound = request(&server.host, "GET", "/api/state", &[("Host", "evil.example"), ("X-Pact-Token", &server.token)], None);
    assert_eq!(rebound.status, 403, "{}", rebound.body);
    assert_eq!(request(&server.host, "GET", "/", &[("Host", "evil.example")], None).status, 403);

    assert!(server.tasks().is_empty());
    let not_a_repo = server.post("/api/repos", &json!({ "path": outside }));
    assert_eq!(not_a_repo.status, 400, "{}", not_a_repo.body);
    let added = server.post("/api/repos", &json!({ "path": repo }));
    assert_eq!(added.status, 200, "{}", added.body);
    let repo_key = added.json()["path"].as_str().unwrap().to_string();
    let saved = std::fs::read_to_string(outside.join("ui-repos.json")).unwrap();
    assert!(saved.contains(repo.file_name().unwrap().to_str().unwrap()), "{saved}");

    let unknown = server.post("/api/tasks", &json!({ "repo": outside, "agent": "claude", "prompt": "x" }));
    assert_eq!(unknown.status, 400, "a repository that was never added is refused: {}", unknown.body);

    let script = json!({
        "writes": { "README.md": "# demo\nmore\n", "notes/new.txt": "NEW\n" },
        "summary": format!("wrote both{DONE_REPORT}"),
    });
    let started = server.post("/api/tasks", &json!({ "repo": repo_key, "name": "Alpha Task", "agent": "claude", "prompt": script.to_string() }));
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.json()["id"], "alpha-task");

    let task = server.wait_for_status("alpha-task", "done");
    assert_eq!(task["branch"], "pact/alpha-task", "{task}");
    assert_eq!(task["agent"], "claude", "{task}");
    assert_eq!(task["dirty"], true, "{task}");
    assert_eq!(task["report"]["verified"], "read them back", "{task}");

    let duplicate = server.post("/api/tasks", &json!({ "repo": repo_key, "name": "alpha task", "agent": "claude", "prompt": "x" }));
    assert_eq!(duplicate.status, 400, "{}", duplicate.body);
    assert!(duplicate.body.contains("already exists"), "{}", duplicate.body);

    let detail = server.get(&format!("/api/task?repo={}&id=alpha-task", query_escape(&repo_key)));
    assert_eq!(detail.status, 200, "{}", detail.body);
    let detail = detail.json();
    let patch = detail["patch"].as_str().unwrap();
    assert_eq!(detail["truncated"], false, "{detail}");
    assert!(patch.contains("diff --git a/README.md b/README.md") && patch.contains("+more"), "the modified file: {patch}");
    assert!(patch.contains("diff --git a/notes/new.txt b/notes/new.txt") && patch.contains("new file mode"), "the untracked file: {patch}");
    assert!(patch.contains("+NEW"), "{patch}");
    let workspace = pact_vcs::WorkspaceManager::state_dir_for(&repo).unwrap().join("workspaces").join("alpha-task");
    assert_eq!(detail["base"], run_git(&repo, &["rev-parse", "HEAD"]).as_str(), "the commit the patch is against: {detail}");
    let reported = PathBuf::from(detail["workspace"].as_str().unwrap());
    assert_eq!(std::fs::canonicalize(reported).unwrap(), std::fs::canonicalize(&workspace).unwrap(), "{detail}");
    assert!(run_git(&workspace, &["status", "--porcelain"]).contains("?? notes/"), "building the patch staged nothing");

    let landed = server.post("/api/land", &json!({ "repo": repo_key, "id": "alpha-task", "pr": false }));
    assert_eq!(landed.status, 200, "{}", landed.body);
    let landed = landed.json();
    assert_eq!(landed["kind"], "fast_forward", "{landed}");
    assert_eq!(landed["target_branch"], "main", "{landed}");
    assert_eq!(std::fs::read_to_string(repo.join("notes").join("new.txt")).unwrap().trim(), "NEW");
    assert!(std::fs::read_to_string(repo.join("README.md")).unwrap().contains("more"));
    assert_eq!(run_git(&repo, &["status", "--porcelain"]), "");
    assert!(server.tasks().is_empty());

    let history = server.get(&format!("/api/history?path={}", query_escape(&repo_key)));
    assert_eq!(history.status, 200, "{}", history.body);
    assert!(history.body.contains("alpha-task"), "{}", history.body);

    let second = server.post("/api/tasks", &json!({ "repo": repo_key, "agent": "claude", "prompt": json!({ "writes": { "b.txt": "B" }, "summary": format!("ok{DONE_REPORT}") }).to_string() }));
    assert_eq!(second.status, 200, "{}", second.body);
    let second_id = second.json()["id"].as_str().unwrap().to_string();
    server.wait_for_status(&second_id, "done");
    let discarded = server.post("/api/discard", &json!({ "repo": repo_key, "id": second_id }));
    assert_eq!(discarded.status, 200, "{}", discarded.body);
    assert!(server.tasks().is_empty());
    assert!(!repo.join("b.txt").exists());

    drop(server);
    let state = pact_vcs::WorkspaceManager::state_dir_for(&repo).unwrap();
    for dir in [&repo, &state, &shim, &outside] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn ui_reply_reruns_the_agent_in_the_same_workspace() {
    let repo = init_repo("reply-repo");
    let shim = shim_dir();
    let outside = scratch_dir("reply-outside");
    let server = Server::start(&outside, &shim, &outside);
    let added = server.post("/api/repos", &json!({ "path": repo }));
    assert_eq!(added.status, 200, "{}", added.body);
    let repo_key = added.json()["path"].as_str().unwrap().to_string();

    // A prompt that is not a script makes the fake agent end without a report.
    let started = server.post("/api/tasks", &json!({ "repo": repo_key, "name": "ask", "agent": "claude", "prompt": "Pick a database and wire it in." }));
    assert_eq!(started.status, 200, "{}", started.body);
    server.wait_for_status("ask", "finished");
    let state_dir = pact_vcs::WorkspaceManager::state_dir_for(&repo).unwrap();
    let workspace = state_dir.join("workspaces").join("ask");
    std::fs::write(workspace.join("earlier.txt"), "from the first run
").unwrap();

    let empty = server.post("/api/reply", &json!({ "repo": repo_key, "id": "ask", "answer": "  " }));
    assert_eq!(empty.status, 400, "{}", empty.body);
    let missing = server.post("/api/reply", &json!({ "repo": repo_key, "id": "nope", "answer": "x" }));
    assert_eq!(missing.status, 400, "{}", missing.body);

    let answer = json!({ "writes": { "answer.txt": "sqlite
" }, "summary": format!("used sqlite{DONE_REPORT}") });
    let replied = server.post("/api/reply", &json!({ "repo": repo_key, "id": "ask", "answer": answer.to_string() }));
    assert_eq!(replied.status, 200, "{}", replied.body);
    let task = server.wait_for_status("ask", "done");
    assert_eq!(task["branch"], "pact/ask", "{task}");
    assert_eq!(task["prompt"], "Pick a database and wire it in.", "the task keeps its original text: {task}");
    assert_eq!(server.tasks().len(), 1);
    assert!(workspace.join("earlier.txt").exists(), "the first run's work is kept");
    assert_eq!(std::fs::read_to_string(workspace.join("answer.txt")).unwrap().trim(), "sqlite");

    let record = std::fs::read_to_string(state_dir.join("meta").join("runs").join("ask.json")).unwrap();
    assert!(record.contains("Pick a database and wire it in."), "the rerun prompt carries the task: {record}");
    assert!(record.contains("fake-agent: done"), "and the agent's last message: {record}");
    assert!(record.contains("The user's reply:"), "{record}");
    assert!(!record.contains("earlier stops"), "a first reply has no thread: {record}");

    let again = server.post("/api/reply", &json!({ "repo": repo_key, "id": "ask", "answer": "Also add an index." }));
    assert_eq!(again.status, 200, "{}", again.body);
    let run_record = state_dir.join("meta").join("runs").join("ask.json");
    let deadline = Instant::now() + Duration::from_secs(90);
    let record = loop {
        let record = std::fs::read_to_string(&run_record).unwrap_or_default();
        if record.contains("Also add an index.") {
            break record;
        }
        assert!(Instant::now() < deadline, "the second reply never ran: {record}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(record.contains("earlier stops") && record.contains("1. {"), "the second reply carries the first: {record}");
    server.wait_for_status("ask", "done");
    let replies = state_dir.join("meta").join("replies").join("ask.json");
    let thread: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&replies).unwrap()).unwrap();
    assert_eq!(thread.len(), 2, "{thread:?}");

    let landed = server.post("/api/land", &json!({ "repo": repo_key, "id": "ask", "pr": false }));
    assert_eq!(landed.status, 200, "{}", landed.body);
    assert!(repo.join("earlier.txt").exists() && repo.join("answer.txt").exists());
    assert!(!replies.exists(), "the thread goes with the workspace");

    drop(server);
    for dir in [&repo, &state_dir, &shim, &outside] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
