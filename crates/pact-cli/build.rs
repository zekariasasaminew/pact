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
}
