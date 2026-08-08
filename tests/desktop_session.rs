//! Display-manager session detection (Task 5.2 of `tessera-bar-login`):
//! after `make install` into a temporary prefix, the installed
//! `tessera.desktop` exists, is parseable, and carries the fields a display
//! manager needs to list Tessera as a login session — without any running
//! GDM/SDDM/LightDM (the DM-side listing is manual E2E).
//!
//! `make` is required to exercise the install flow; when it is absent (a
//! trimmed toolchain) the test degrades to asserting the same artifact as it
//! is checked into the repo (`install/tessera.desktop`), which is the file
//! `make install` copies verbatim (design D8: static, never build-generated).

use std::path::Path;
use std::process::Command;

/// The workspace root (the binary package's manifest dir is the repo root).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Whether `make` (GNU make, as required by the Makefile) is on `PATH`.
fn make_available() -> bool {
    Command::new("make")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Asserts a freedesktop `.desktop` file parses to the XDG session-entry
/// contract: `Type=Application`, `Exec=tessera`, and (for the session
/// selector) `Name=Tessera`. Only those keys are read — comment/preamble and
/// the `[Desktop Entry]` header are skipped.
fn assert_session_entry(contents: &str, source: &Path) {
    let mut has_type = false;
    let mut has_exec = false;
    let mut has_name = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Type" => has_type = value.trim() == "Application",
                "Exec" => has_exec = value.trim() == "tessera",
                "Name" => has_name = value.trim() == "Tessera",
                _ => {}
            }
        }
    }
    assert!(
        has_type,
        "{}: a session entry needs `Type=Application`, got:\n{contents}",
        source.display()
    );
    assert!(
        has_exec,
        "{}: a session entry needs `Exec=tessera`, got:\n{contents}",
        source.display()
    );
    assert!(
        has_name,
        "{}: the session selector needs `Name=Tessera`, got:\n{contents}",
        source.display()
    );
}

#[test]
fn installed_desktop_entry_is_parseable_and_registers_the_tessera_session() {
    let source = repo_root().join("install").join("tessera.desktop");

    if !make_available() {
        // No `make`: still prove the checked-in artifact satisfies the XDG
        // contract (this is exactly the file `make install` copies).
        eprintln!("make not found; asserting the checked-in install/tessera.desktop");
        assert_session_entry(
            &std::fs::read_to_string(&source).expect("read install/tessera.desktop"),
            &source,
        );
        return;
    }

    let prefix = std::env::temp_dir().join(format!("tessera-dm-prefix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&prefix);

    let install = Command::new("make")
        .current_dir(repo_root())
        .env("PREFIX", &prefix)
        .arg("install")
        .output()
        .expect("run `make install`");
    assert!(
        install.status.success(),
        "`make install PREFIX={}` failed:\n{}",
        prefix.display(),
        String::from_utf8_lossy(&install.stderr)
    );

    let desktop = prefix
        .join("share")
        .join("xsessions")
        .join("tessera.desktop");
    assert!(
        desktop.exists(),
        "`make install` must place {} (stdout: {})",
        desktop.display(),
        String::from_utf8_lossy(&install.stdout)
    );

    // The installed file must be freshly copied (not the repo file's stale
    // twin by accident) and parse with the XDG contract.
    let contents = std::fs::read_to_string(&desktop).expect("read installed tessera.desktop");
    assert_session_entry(&contents, &desktop);

    // `Exec=tessera` resolves against the installed binary (the DM launches
    // the WM by name; a bare package with no binary would 404 at login).
    assert!(
        prefix.join("bin").join("tessera").exists(),
        "`make install` must place the binary; `Exec=tessera` has nothing to run"
    );

    let _ = std::fs::remove_dir_all(&prefix);
}
