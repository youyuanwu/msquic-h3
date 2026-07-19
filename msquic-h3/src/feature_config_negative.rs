#[test]
#[ignore = "NEGATIVE config check: spawns a nested `cargo check` expected to FAIL; run manually with --ignored"]
fn both_features_mutually_exclusive_negative() {
    use std::process::Command;

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // Isolated target dir so this nested invocation does not contend for the
    // outer `cargo test` build lock.
    let neg_target = format!("{manifest_dir}/target/neg-both-features");

    let output = Command::new(&cargo)
        .current_dir(manifest_dir)
        .env("CARGO_TARGET_DIR", &neg_target)
        .args([
            "check",
            "--no-default-features",
            "--features",
            "native-find,native-src",
        ])
        .output()
        .expect("failed to spawn nested cargo check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "enabling BOTH native-find + native-src MUST fail the build; got success.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("feature src and find are mutually exclusive"),
        "expected the upstream msquic build-script mutual-exclusion message; \
         the failure must originate upstream (not a crate-level guard).\nstderr:\n{stderr}"
    );
}
