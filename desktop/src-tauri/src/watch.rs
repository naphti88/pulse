// Reads again when the files behind Pulse change, instead of on a short
// timer. Changes come in bursts while a chat runs (its transcript grows line
// by line): they are gathered for a moment and answered with one refresh. A
// slow tick catches what time alone changes (an age, a missed slot, the morning).

use std::sync::mpsc::channel;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tauri::AppHandle;

use crate::util::paths;

const GATHER: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_secs(30);

pub fn start(app: AppHandle, refresh: fn(&AppHandle)) -> Result<(), String> {
    let p = paths()?;
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;
    for (path, mode) in [
        (&p.app, RecursiveMode::Recursive),
        // its runs and what is pending now, also when one of them comes later
        (&p.recorder, RecursiveMode::Recursive),
        (&p.projects, RecursiveMode::Recursive),
        (&p.leveldb, RecursiveMode::NonRecursive),
    ] {
        // a folder that is missing now is reported by the inputs as a problem
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
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        refresh(&app);
    });
    Ok(())
}
