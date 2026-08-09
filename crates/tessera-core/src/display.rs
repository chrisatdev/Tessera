//! Display seam (design D1/D2): the trait that keeps the X11 layer outside
//! the pure core, plus the headless [`MockDisplay`] test double.
//!
//! [`DisplayServer`] is the only X11 boundary of `tessera-core`. The event
//! loop drives it; `tessera-x11` (U4) implements it over x11rb. D2: events
//! are translated to [`Event`]s by the X layer, so the loop's races are
//! scriptable headless through `MockDisplay`.

use std::fmt;
use std::process::Stdio;

use crate::event::Event;
use crate::geometry::{Rect, WindowId};

/// Identifier of a reparenting frame window created by
/// [`DisplayServer::manage`] (REQ-x11-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameId(pub u32);

/// Errors from the display layer (U1-style enum carrying strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DErr {
    /// X connection/protocol failure (connect, claim, event read, ...).
    X(String),
    /// A child process could not be spawned (T13).
    Spawn(String),
}

impl fmt::Display for DErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DErr::X(msg) => write!(f, "x11: {msg}"),
            DErr::Spawn(msg) => write!(f, "spawn: {msg}"),
        }
    }
}

impl std::error::Error for DErr {}

/// The X11 seam (design D1): every side effect of the window manager, as a
/// trait the pure core can drive. Methods return [`DErr`] so the loop can log
/// a failure and keep running instead of panicking.
pub trait DisplayServer {
    /// Opens the display connection (REQ-x11-001, wired in U4).
    fn connect(&mut self) -> Result<(), DErr>;
    /// Claims the `WM_S0` selection before managing (REQ-x11-002, U4).
    fn claim_wm(&mut self) -> Result<(), DErr>;
    /// Blocks for the next translated event; `None` means the connection
    /// closed (D2: translation is the X layer's job, REQ-x11-004).
    fn next_event(&mut self) -> Result<Option<Event>, DErr>;
    /// Reparents client `w` into a fresh frame and returns its id
    /// (REQ-x11-005, SC-x11-07).
    fn manage(&mut self, w: WindowId) -> Result<FrameId, DErr>;
    /// Maps the frame of client `w`.
    fn map_window(&mut self, w: WindowId) -> Result<(), DErr>;
    /// Unmaps the frame of client `w` (workspace switch, SC-ws-06).
    fn unmap_window(&mut self, w: WindowId) -> Result<(), DErr>;
    /// Resizes/moves client `w`'s frame to `r` (REQ-x11-006, SC-x11-09).
    fn configure(&mut self, w: WindowId, r: Rect) -> Result<(), DErr>;
    /// Sets X input focus to client `w`.
    fn focus_window(&mut self, w: WindowId) -> Result<(), DErr>;
    /// Destroys frame `f` (the display-layer reaction to `WindowUnmapped`).
    fn destroy_frame(&mut self, f: FrameId) -> Result<(), DErr>;
    /// Syncs the three EWMH desktop properties (recorded only until U4,
    /// REQ-ws-003 / SC-ws-05).
    fn set_desktops(&mut self, n: u32, cur: u32, names: &[String]) -> Result<(), DErr>;
    /// Spawns `prog` resolved through PATH, without a shell (T13). Default
    /// implementation delegates to [`DisplayServer::spawn_with_args`] with a
    /// single-entry argv, so every existing call site keeps working (D3).
    fn spawn(&self, prog: &str) -> Result<(), DErr> {
        self.spawn_with_args(&[prog.to_string()])
    }
    /// Spawns `argv` with verbatim entries: the program is resolved through
    /// PATH by [`std::process::Command`] — no shell, no string
    /// interpretation, no argument splitting — and stdio is detached (null).
    /// A failure is returned as [`DErr::Spawn`] so the caller logs it and the
    /// loop keeps running (ALA-1, D3).
    fn spawn_with_args(&self, argv: &[String]) -> Result<(), DErr>;
}

/// Spawns `prog` as a child process (T13, process boundary).
///
/// Kept as a thin wrapper over [`spawn_program_args`] (design D3) so the
/// single-program call sites keep their `&str` signature.
pub fn spawn_program(prog: &str) -> Result<(), DErr> {
    spawn_program_args(&[prog.to_string()])
}

/// Spawns `argv` as a child process with verbatim entries (ALA-1, D3).
///
/// The first entry is resolved through `PATH` by [`std::process::Command`] —
/// no shell, no string interpretation, no argument injection — and every
/// entry is passed through unchanged: an argument containing spaces or shell
/// metacharacters stays ONE argument (never split, never interpreted). The
/// stdio is detached (`null`): a spawned GUI program must never inherit the
/// WM's (or a test harness's) pipes. The child is not waited on. An empty
/// argv (nothing to exec) and a bogus program are both returned as
/// [`DErr::Spawn`] so the caller logs them and the loop keeps running.
pub fn spawn_program_args(argv: &[String]) -> Result<(), DErr> {
    let Some(prog) = argv.first() else {
        return Err(DErr::Spawn("empty argv".to_string()));
    };
    std::process::Command::new(prog)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| DErr::Spawn(err.to_string()))
}

#[cfg(test)]
pub(crate) mod test_double {
    //! Headless [`DisplayServer`] for loop tests (the U3 harness).
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use super::{DErr, DisplayServer, FrameId, spawn_program_args};
    use crate::event::Event;
    use crate::geometry::{Rect, WindowId};

    /// One recorded display call, in order (manage -> map -> configure ->
    /// focus). Tests assert the loop's behavior on this log.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum DisplayCall {
        Connect,
        ClaimWm,
        Manage(WindowId),
        Map(WindowId),
        Unmap(WindowId),
        Configure(WindowId, Rect),
        Focus(WindowId),
        DestroyFrame(FrameId),
        SetDesktops(u32, u32, Vec<String>),
        Spawn(Vec<String>),
    }

    /// Yields scripted [`Event`]s through [`DisplayServer::next_event`] and
    /// records every call into a log shared with the test (the loop owns the
    /// display, so the test inspects the log through the shared `Arc`).
    pub(crate) struct MockDisplay {
        script: VecDeque<Event>,
        calls: Arc<Mutex<Vec<DisplayCall>>>,
        frames: HashMap<WindowId, FrameId>,
        next_frame: u32,
    }

    impl MockDisplay {
        /// Creates a double that yields `script` then `Ok(None)`, returning
        /// the shared call log alongside it.
        pub(crate) fn new(script: Vec<Event>) -> (Self, Arc<Mutex<Vec<DisplayCall>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                MockDisplay {
                    script: script.into(),
                    calls: Arc::clone(&calls),
                    frames: HashMap::new(),
                    next_frame: 1,
                },
                calls,
            )
        }

        /// Frame id assigned to client `w` by the last `manage`.
        pub(crate) fn frame_of(&self, w: WindowId) -> Option<FrameId> {
            self.frames.get(&w).copied()
        }

        /// Snapshot of the recorded calls, in order.
        pub(crate) fn calls(&self) -> Vec<DisplayCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DisplayServer for MockDisplay {
        fn connect(&mut self) -> Result<(), DErr> {
            self.calls.lock().unwrap().push(DisplayCall::Connect);
            Ok(())
        }
        fn claim_wm(&mut self) -> Result<(), DErr> {
            self.calls.lock().unwrap().push(DisplayCall::ClaimWm);
            Ok(())
        }
        fn next_event(&mut self) -> Result<Option<Event>, DErr> {
            Ok(self.script.pop_front())
        }
        fn manage(&mut self, w: WindowId) -> Result<FrameId, DErr> {
            self.calls.lock().unwrap().push(DisplayCall::Manage(w));
            let frame = FrameId(self.next_frame);
            self.next_frame += 1;
            self.frames.insert(w, frame);
            Ok(frame)
        }
        fn map_window(&mut self, w: WindowId) -> Result<(), DErr> {
            self.calls.lock().unwrap().push(DisplayCall::Map(w));
            Ok(())
        }
        fn unmap_window(&mut self, w: WindowId) -> Result<(), DErr> {
            self.calls.lock().unwrap().push(DisplayCall::Unmap(w));
            Ok(())
        }
        fn configure(&mut self, w: WindowId, r: Rect) -> Result<(), DErr> {
            self.calls
                .lock()
                .unwrap()
                .push(DisplayCall::Configure(w, r));
            Ok(())
        }
        fn focus_window(&mut self, w: WindowId) -> Result<(), DErr> {
            self.calls.lock().unwrap().push(DisplayCall::Focus(w));
            Ok(())
        }
        fn destroy_frame(&mut self, f: FrameId) -> Result<(), DErr> {
            self.calls
                .lock()
                .unwrap()
                .push(DisplayCall::DestroyFrame(f));
            Ok(())
        }
        fn set_desktops(&mut self, n: u32, cur: u32, names: &[String]) -> Result<(), DErr> {
            self.calls
                .lock()
                .unwrap()
                .push(DisplayCall::SetDesktops(n, cur, names.to_vec()));
            Ok(())
        }
        fn spawn_with_args(&self, argv: &[String]) -> Result<(), DErr> {
            self.calls
                .lock()
                .unwrap()
                .push(DisplayCall::Spawn(argv.to_vec()));
            spawn_program_args(argv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_double::{DisplayCall, MockDisplay};
    use super::*;

    #[test]
    fn mock_records_connect_claim_and_ewmh_sync_calls() {
        // Trait surface: every method is recordable headless; the EWMH sync
        // is recorded only until U4 (SC-ws-05 seam).
        let (mut d, _log) = MockDisplay::new(Vec::new());
        d.connect().unwrap();
        d.claim_wm().unwrap();
        d.set_desktops(2, 1, &["1".to_string(), "2".to_string()])
            .unwrap();
        assert_eq!(
            d.calls(),
            vec![
                DisplayCall::Connect,
                DisplayCall::ClaimWm,
                DisplayCall::SetDesktops(2, 1, vec!["1".to_string(), "2".to_string()]),
            ]
        );
    }

    #[test]
    fn mock_assigns_distinct_frame_ids_per_window() {
        let (mut d, _log) = MockDisplay::new(Vec::new());
        let f1 = d.manage(10).unwrap();
        let f2 = d.manage(20).unwrap();
        assert_eq!(d.frame_of(10), Some(f1));
        assert_eq!(d.frame_of(20), Some(f2));
        assert_ne!(f1, f2);
        assert_eq!(
            d.calls(),
            vec![DisplayCall::Manage(10), DisplayCall::Manage(20)]
        );
    }

    #[test]
    fn mock_script_exhausts_to_none() {
        let (mut d, _log) = MockDisplay::new(vec![Event::WindowMapRequested(1)]);
        assert_eq!(d.next_event().unwrap(), Some(Event::WindowMapRequested(1)));
        assert_eq!(d.next_event().unwrap(), None);
    }

    #[test]
    fn spawn_program_runs_an_absolute_path_without_a_shell() {
        // A known binary via absolute path: spawned directly, no shell.
        assert!(spawn_program("/bin/true").is_ok());
    }

    #[test]
    fn spawn_program_uses_path_only_lookup() {
        // A program that does not exist on PATH fails cleanly.
        assert!(matches!(
            spawn_program("tessera-no-such-program-xyz"),
            Err(DErr::Spawn(_))
        ));
    }

    #[test]
    fn spawn_program_never_uses_a_shell() {
        // Shell metacharacters are treated as a program NAME, never executed:
        // the spawn must fail and nothing may be created. This proves there
        // is no shell interpretation of the string.
        let payload = "/tmp/opencode/tessera-no-shell-pwned";
        let _ = std::fs::remove_file(payload);
        let cmd = format!("echo hi > {payload}");
        assert!(matches!(spawn_program(&cmd), Err(DErr::Spawn(_))));
        assert!(!std::path::Path::new(payload).exists());
    }

    // === spawn_program_args — PR3 / WU3 (tessera-keybinds-launcher) ===

    #[test]
    fn spawn_program_args_rejects_empty_argv() {
        // ALA-1 / design D3: an empty argv has no program to exec — a
        // misconfiguration that would silently spawn nothing. Must error.
        assert!(matches!(
            spawn_program_args(&[]),
            Err(DErr::Spawn(_))
        ));
    }

    #[test]
    fn spawn_program_args_passes_argv_verbatim_without_a_shell() {
        // ALA-1 "No shell interpretation": argv entries are passed VERBATIM
        // to the program — `>` inside an argument is data, never redirection.
        // echo prints it literally and NO file is created. (Task list 3.1:
        // "no shell, no file" — the args-aware form succeeds; the string
        // form above still errors.)
        let payload = "/tmp/opencode/tessera-argv-no-shell-pwned";
        let _ = std::fs::remove_file(payload);
        assert!(spawn_program_args(&[
            "echo".to_string(),
            format!("hi > {payload}"),
        ])
        .is_ok());
        assert!(!std::path::Path::new(payload).exists());
    }

    #[test]
    fn spawn_program_args_uses_path_only_lookup() {
        // A program that does not exist on PATH fails cleanly with DErr::Spawn.
        assert!(matches!(
            spawn_program_args(&["tessera-no-such-program-xyz".to_string()]),
            Err(DErr::Spawn(_))
        ));
    }

    #[test]
    fn spawn_program_args_runs_an_absolute_path_without_a_shell() {
        // A known binary via absolute path: spawned directly, no shell.
        assert!(spawn_program_args(&["/bin/true".to_string()]).is_ok());
    }
}
