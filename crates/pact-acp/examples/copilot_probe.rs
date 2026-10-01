//! Manual interop check against the real Copilot CLI, run by hand (never
//! from the test suite): `cargo run -p pact-acp --example copilot_probe`.
//! Starts one `copilot --acp` with a lean COPILOT_HOME, opens two sessions
//! in two temp dirs, prompts both concurrently to write a file, and prints
//! what came back. Requires a logged-in Copilot CLI on PATH.

use std::sync::Arc;

use pact_acp::{AcpRuntime, AgentSpec};

fn main() {
    let user_home = home_dir().join(".copilot");
    let lean = std::env::temp_dir().join(format!("pact-acp-probe-home-{}", std::process::id()));
    std::fs::create_dir_all(&lean).unwrap();
    for file in ["config.json", "settings.json"] {
        let _ = std::fs::copy(user_home.join(file), lean.join(file));
    }
    std::fs::write(lean.join("mcp-config.json"), "{\"mcpServers\":{}}\n").unwrap();

    let program = if cfg!(windows) { "copilot.cmd" } else { "copilot" };
    let spec = AgentSpec {
        program: program.into(),
        args: ["--acp", "--allow-all-tools", "--disable-builtin-mcps", "--log-level", "error"].map(String::from).to_vec(),
        env: vec![("COPILOT_HOME".into(), lean.to_string_lossy().to_string())],
        cwd: None,
    };
    let started = std::time::Instant::now();
    let runtime = Arc::new(AcpRuntime::start_unattended(spec).expect("start copilot --acp"));
    let init = runtime.initialize_result();
    println!(
        "initialize in {:.1}s: agent={:?} caps={}",
        started.elapsed().as_secs_f32(),
        init.agent_info.as_ref().map(|a| format!("{} {}", a.name, a.version)),
        init.agent_capabilities
    );

    let handles: Vec<_> = (0..2)
        .map(|i| {
            let runtime = runtime.clone();
            std::thread::spawn(move || {
                let dir = std::env::temp_dir().join(format!("pact-acp-probe-lane{i}-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let t = std::time::Instant::now();
                let mut session = runtime.new_session(&dir, Vec::new()).expect("session/new");
                println!(
                    "lane {i}: session {} in {:.1}s, config options: {:?}",
                    session.id,
                    t.elapsed().as_secs_f32(),
                    session.config_options.iter().filter_map(|o| o.get("id").and_then(|v| v.as_str())).collect::<Vec<_>>()
                );
                let t = std::time::Instant::now();
                let mut kinds = std::collections::BTreeMap::new();
                let mut text = String::new();
                let prompt = format!(
                    "Create a file named hello.txt in the current working directory containing exactly: hello from lane {i}. \
                     Do not ask questions. Reply DONE."
                );
                let stop = runtime
                    .prompt(&mut session, &prompt, |u| {
                        *kinds.entry(u.kind.clone()).or_insert(0) += 1;
                        if let Some(t) = u.text() {
                            text.push_str(t);
                        }
                    })
                    .expect("prompt");
                let content = std::fs::read_to_string(dir.join("hello.txt")).ok();
                println!("lane {i}: {stop:?} in {:.1}s, file={content:?}, reply={text:?}, updates={kinds:?}", t.elapsed().as_secs_f32());
                runtime.close(&session).expect("close");
                let _ = std::fs::remove_dir_all(&dir);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    Arc::try_unwrap(runtime).ok().unwrap().shutdown();
    let _ = std::fs::remove_dir_all(&lean);
    println!("total {:.1}s", started.elapsed().as_secs_f32());
}

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(Into::into).expect("home dir")
}
