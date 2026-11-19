fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let full_version = match std::env::var("PACT_EDGE_SHA") {
        Ok(sha) if !sha.is_empty() => {
            let short = &sha[..sha.len().min(7)];
            format!("{version}-edge.{short}")
        }
        _ => version,
    };
    println!("cargo:rustc-env=PACT_VERSION={full_version}");
    println!("cargo:rerun-if-env-changed=PACT_EDGE_SHA");

    // Windows gives the main thread 1 MiB; `main`'s debug-build frame
    // outgrew it (issue #461). 8 MiB matches the usual Unix default.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => println!("cargo:rustc-link-arg-bins=/STACK:8388608"),
            _ => println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608"),
        }
    }
}
