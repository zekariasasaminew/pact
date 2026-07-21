//! Integration coverage for issue #34: `pact --version`/`-V` used to fail
//! with `error: unexpected argument '--version' found` because the
//! top-level `Cli` derive had no `version` attribute. Drives the real
//! built `pact` binary directly, since this is exactly the kind of
//! clap-wiring behavior that can't be exercised by calling Rust functions
//! directly.
use std::process::Command;

#[test]
fn version_flag_prints_the_version_and_exits_0() {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pact"), "expected version output to name the binary, got: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected version output to contain the crate version {}, got: {stdout}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn short_version_flag_prints_the_version_and_exits_0() {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("-V")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
