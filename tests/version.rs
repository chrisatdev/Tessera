#[test]
fn binary_version_exits_successfully() {
    // Locate the compiled binary in the standard target/debug location.
    let mut bin = std::path::PathBuf::from("target/debug/tessera");
    if !bin.exists() {
        // Fallback to CARGO_TARGET_DIR if the relative path does not exist.
        if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
            bin = std::path::PathBuf::from(dir).join("debug").join("tessera");
        } else {
            // Fallback based on the Cargo manifest location (two parents up).
            bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("target")
                .join("debug")
                .join("tessera");
        }
    }
    assert!(bin.exists(), "binary not built: {:?}", bin);
    let output = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .expect("failed to execute tessera binary");
    assert!(
        output.status.success(),
        "binary exited with non-zero status"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The version string should start with "tessera " and contain the crate version.
    assert!(
        stdout.starts_with("tessera "),
        "unexpected version output: {stdout}"
    );
}
