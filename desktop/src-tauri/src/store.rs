// What Pulse keeps of its own: what the person marked as seen or done, which
// notifications went out for which item, and the day of the last morning
// summary. One JSON file in the app's support folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::checks::Flag;
use crate::util::{now_ms, support_dir};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum MarkKind {
    Seen,
    Done,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Mark {
    pub kind: MarkKind,
    pub at: f64,
    // how far the item had come when it was marked: what came after is not covered
    #[serde(default)]
    pub shown: Option<f64>,
}

// a topic set aside, with a copy of what it was, so that it outlives its
// session in the app
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Parked {
    pub at: f64,
    pub title: String,
    pub project: String,
    pub said: String,
}

// what was told about an item: the state it was in, since when, and how many
// reminders went out while it lay there
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Told {
    pub state: String,
    pub since: f64,
    pub reminders: u32,
}

// a command marked as known: for what it was worth a look then, and when;
// it holds until the next reminder step of that flag
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Known {
    pub flag: Option<Flag>,
    pub at: f64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Store {
    pub marks: HashMap<String, Mark>,
    pub told: HashMap<String, Told>,
    // the local day ("2026-10-29") of the last morning summary
    pub last_summary: Option<String>,
    // false until the first look: what lies around then is taken as told
    pub is_started: bool,
    // commands the person marked as known ("<session>/<id>"), while they run
    // (the earlier "known", a plain list kept until a restart, is dropped)
    pub known_at: HashMap<String, Known>,
    // topics set aside, by session
    pub parked: HashMap<String, Parked>,
    // taken up again in the panel while the session's last answer still says parked
    pub resumed: HashMap<String, f64>,
    // answers that came with nobody at the app, by session (items.rs)
    pub unseen: HashMap<String, f64>,
    // where the person moved the panel (its top left, in pixels of the screen);
    // none: below the menu bar item
    pub panel_at: Option<[i32; 2]>,
}

// a file that could not be read says so once, for the panel's foot
static PROBLEM: Mutex<Option<String>> = Mutex::new(None);
// the file could be neither read nor kept: nothing may overwrite it
static LOCKED: AtomicBool = AtomicBool::new(false);

pub static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(load()));

fn path() -> Result<PathBuf, String> {
    Ok(support_dir()?.join("state.json"))
}

pub fn problem() -> Option<String> {
    PROBLEM.lock().expect("store problem").clone()
}

fn load() -> Store {
    let Ok(path) = path() else { return Store::default() };
    let (store, read) = load_from(&path, now_ms() as i64);
    if let Err(problem) = read {
        eprintln!("store: {}", problem.text);
        LOCKED.store(problem.locked, Ordering::SeqCst);
        *PROBLEM.lock().expect("store problem") = Some(problem.text);
    }
    store
}

struct LoadProblem {
    text: String,
    locked: bool,
}

// what cannot be read starts empty, and the next save would write over it:
// marks and parked topics would be gone for good. So a file that cannot be
// read is kept beside it first, and if even that fails nothing is saved
fn load_from(path: &Path, now: i64) -> (Store, Result<(), LoadProblem>) {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Store::default(), Ok(())),
        Err(_) => {
            let text = "Your marks could not be read. Pulse will not save any until that is solved.".to_string();
            return (Store::default(), Err(LoadProblem { text, locked: true }));
        }
    };
    if let Ok(store) = serde_json::from_slice(&text) {
        return (store, Ok(()));
    }
    let kept = path.with_file_name(format!("state.broken-{now}.json"));
    let problem = match std::fs::write(&kept, text) {
        Ok(()) => LoadProblem { text: format!("Your marks could not be read and start empty. The old file is kept as {}.", kept.display()), locked: false },
        Err(_) => LoadProblem { text: "Your marks could not be read. Pulse will not save any until that is solved.".to_string(), locked: true },
    };
    (Store::default(), Err(problem))
}

pub fn save(store: &Store) -> Result<(), String> {
    if LOCKED.load(Ordering::SeqCst) {
        return Err("The state file could not be read: nothing is saved over it.".into());
    }
    save_to(store, &path()?)
}

// through a tmp file of this process's own: two Pulses saving at once (a dev
// build beside the installed one) each move a whole file into place. A tmp
// file a save could not move is removed
fn save_to(store: &Store, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pulse-store-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_save_leaves_another_pulses_save_alone_and_no_tmp_behind() {
        let dir = folder("tmp");
        let path = dir.join("state.json");
        // another Pulse between its write and its rename: a build from before
        // the tmp files of their own, and one after
        let theirs = [dir.join("state.json.tmp"), dir.join(format!("state.json.{}.tmp", std::process::id() + 1))];
        for tmp in &theirs {
            std::fs::write(tmp, "{
  \"marks\": {").expect("theirs");
        }
        let store = Store { is_started: true, panel_at: Some([100, 200]), ..Store::default() };
        save_to(&store, &path).expect("save");
        let (read, ok) = load_from(&path, 1);
        assert!(ok.is_ok());
        assert_eq!((read.is_started, read.panel_at), (true, Some([100, 200])));
        for tmp in &theirs {
            assert_eq!(std::fs::read_to_string(tmp).expect("theirs"), "{
  \"marks\": {");
        }
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 3);

        // a save that cannot be moved into place takes its tmp file with it
        std::fs::remove_file(&path).expect("saved");
        std::fs::create_dir(&path).expect("in the way");
        assert!(save_to(&store, &path).is_err());
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_there_is_a_first_start() {
        let dir = folder("first");
        let (store, read) = load_from(&dir.join("state.json"), 1);
        assert!(read.is_ok() && store.marks.is_empty());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn what_was_saved_comes_back() {
        let dir = folder("back");
        let path = dir.join("state.json");
        let mut store = Store::default();
        store.parked.insert("a".into(), Parked { at: 1.0, title: "RAM".into(), project: "shop-api".into(), said: "Stand".into() });
        save_to(&store, &path).expect("saved");
        let (back, read) = load_from(&path, 1);
        assert!(read.is_ok());
        assert_eq!(back.parked.get("a"), store.parked.get("a"));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_kept_before_anything_is_saved_over_it() {
        let dir = folder("broken");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json, but your marks").expect("write");
        let (store, read) = load_from(&path, 7);
        let problem = read.err().expect("a problem");
        assert!(store.marks.is_empty() && !problem.locked);
        assert!(problem.text.contains("state.broken-7.json"));
        // the next save writes over state.json: the copy stays
        save_to(&Store::default(), &path).expect("saved");
        assert_eq!(std::fs::read_to_string(dir.join("state.broken-7.json")).expect("kept"), "{ not json, but your marks");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_file_that_cannot_be_kept_locks_saving() {
        let dir = folder("locked");
        // a folder where the file should be: it can be neither read nor copied
        let path = dir.join("state.json");
        std::fs::create_dir(&path).expect("dir");
        let (_, read) = load_from(&path, 7);
        assert!(read.err().expect("a problem").locked);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
