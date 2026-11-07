// What Claude Code says of itself: every running session keeps a record of its
// process in ~/.claude/sessions/<pid>.json, with a status (busy, waiting, idle)
// and, while it waits, for what (a permission prompt, input needed). This is the
// session's own word, at once and for every tool, where the transcript can only
// be read for hints. It comes first (rules.rs).
//
// A session with no live process cannot be in a turn, whatever its transcript's
// end looks like: it crashed, or the app was closed. The files are no documented
// interface: drift.rs says when they change, and without them Pulse goes by the
// transcript as before.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

pub const INPUT_NEEDED: &str = "input needed";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    session_id: Option<String>,
    host_session_id: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    pid: Option<i32>,
    status_updated_at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Engine {
    pub status: String,
    pub waiting_for: Option<String>,
    pub since: f64,
}

impl Engine {
    pub fn is_in_turn(&self) -> bool {
        self.status != "idle"
    }

    pub fn is_waiting(&self) -> bool {
        self.status == "waiting"
    }
}

#[derive(Default)]
pub struct Engines {
    // the records can be read: a session without one has no process
    pub known: bool,
    // files are there, and none says a status: their format changed
    pub changed: bool,
    // by the session's id in its transcript's name and by the app's id
    pub by_id: HashMap<String, Engine>,
}

#[cfg(unix)]
fn is_alive(pid: i32) -> bool {
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

// on Windows: a process that can be opened and has not exited; one that may
// not be opened is there all the same
#[cfg(windows)]
fn is_alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid <= 0 {
        return false;
    }
    // SAFETY: plain queries; the handle is closed before returning
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if process.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(process);
        running
    }
}

pub fn engines_in(dir: &Path, alive: impl Fn(i32) -> bool) -> Engines {
    let Ok(entries) = std::fs::read_dir(dir) else { return Engines::default() };
    let mut engines = Engines::default();
    let (mut files, mut with_status) = (0, 0);
    for path in entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        files += 1;
        let Ok(record) = serde_json::from_str::<Record>(&text) else { continue };
        let Some(status) = record.status else { continue };
        with_status += 1;
        // a record left behind by a process that is gone
        if !record.pid.is_some_and(&alive) {
            continue;
        }
        let engine = Engine { status, waiting_for: record.waiting_for, since: record.status_updated_at.unwrap_or(0.0) };
        for id in [record.session_id, record.host_session_id].into_iter().flatten() {
            engines.by_id.insert(id, engine.clone());
        }
    }
    engines.changed = files > 0 && with_status == 0;
    engines.known = !engines.changed;
    engines
}

pub fn engines(dir: &Path) -> Engines {
    engines_in(dir, is_alive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pulse-engine-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        for (file, text) in files {
            std::fs::write(dir.join(file), text).expect("write");
        }
        dir
    }

    const BUSY: &str = r#"{"sessionId":"c1","hostSessionId":"local_1","status":"busy","pid":10,"statusUpdatedAt":5}"#;
    const WAITS: &str = r#"{"sessionId":"c2","hostSessionId":"local_2","status":"waiting","waitingFor":"permission prompt","pid":11,"statusUpdatedAt":6}"#;

    #[test]
    fn a_record_says_what_a_session_does_by_either_id() {
        let dir = folder("ids", &[("10.json", BUSY), ("11.json", WAITS), ("11.abc.key", "x")]);
        let found = engines_in(&dir, |_| true);
        assert!(found.known && !found.changed);
        assert_eq!(found.by_id["c1"].status, "busy");
        assert_eq!(found.by_id["local_2"], Engine { status: "waiting".into(), waiting_for: Some("permission prompt".into()), since: 6.0 });
        assert!(found.by_id["local_2"].is_waiting() && found.by_id["c1"].is_in_turn());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_process_that_is_gone_has_no_session() {
        let dir = folder("gone", &[("10.json", BUSY), ("11.json", WAITS)]);
        let found = engines_in(&dir, |pid| pid == 11);
        assert!(found.known && !found.by_id.contains_key("c1") && found.by_id.contains_key("c2"));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn nothing_running_is_known_and_files_without_a_status_are_not() {
        let empty = folder("empty", &[]);
        assert!(engines_in(&empty, |_| true).known);
        std::fs::remove_dir_all(&empty).expect("clean up");
        assert!(!engines_in(Path::new("/no/such/folder"), |_| true).known);
        let old = folder("old", &[("10.json", r#"{"sessionId":"c1","pid":10}"#)]);
        let found = engines_in(&old, |_| true);
        assert!(found.changed && !found.known);
        std::fs::remove_dir_all(&old).expect("clean up");
    }
}
