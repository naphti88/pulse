// Reads again when the files behind Pulse change, instead of on a short
// timer. Changes come in bursts while a chat runs (its transcript grows line
// by line): they are gathered for a moment and answered with one refresh. A
// slow tick catches what time alone changes (an age, a missed slot, the morning).
// Codex is checked every few seconds besides: the watcher misses what it
// appends to a rollout it holds open (on Windows at least), and a turn read or
// a thread renamed changes only its database.

use std::sync::mpsc::channel;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tauri::AppHandle;

use crate::codex;
use crate::util::paths;

const GATHER: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_secs(30);
const CHECK: Duration = Duration::from_secs(5);

pub fn start(app: AppHandle, refresh: fn(&AppHandle)) -> Result<(), String> {
    let p = paths()?;
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;
    for (path, mode) in [
        (&p.app, RecursiveMode::Recursive),
        // its runs and what is pending now, also when one of them comes later
        (&p.recorder, RecursiveMode::Recursive),
        (&p.engines, RecursiveMode::NonRecursive),
        (&p.projects, RecursiveMode::Recursive),
        (&p.leveldb, RecursiveMode::NonRecursive),
        (&p.codex.join("sessions"), RecursiveMode::Recursive),
    ] {
        // a folder that is missing now is reported by the inputs as a problem;
        // a Codex that comes later is found by its check
        if path.exists() {
            watcher.watch(path, mode).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    let on_change = app.clone();
    std::thread::spawn(move || {
        let _watcher = watcher;
        while rx.recv().is_ok() {
            std::thread::sleep(GATHER);
            while rx.try_recv().is_ok() {}
            refresh(&on_change);
        }
    });
    let (folder, on_codex) = (p.codex, app.clone());
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            std::thread::sleep(CHECK);
            // a database that can't be read now is left for the next check
            let Some(token) = codex::token(&folder) else { continue };
            if last.is_some_and(|last| last != token) {
                refresh(&on_codex);
            }
            last = Some(token);
        }
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        refresh(&app);
    });
    Ok(())
}
