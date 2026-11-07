// Where the app and the routine-recorder keep their files, and the small
// helpers every reader shares.

use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub fn home() -> Result<PathBuf, String> {
    // started from Explorer, Windows sets no HOME, only USERPROFILE
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).ok_or_else(|| "HOME is not set.".to_string())
}

// where Pulse keeps its state and settings
pub fn support_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("local.claude-dashboard.pulse")).ok_or_else(|| "APPDATA is not set.".to_string());
    #[cfg(not(windows))]
    return Ok(home()?.join("Library/Application Support/local.claude-dashboard.pulse"));
}

// the Claude app's own folder: on Windows the packaged app (the download and
// the Store build alike) keeps it under %LOCALAPPDATA%\Claude-Data\<package>,
// the old installer under %APPDATA%\Claude. A build left behind or installed
// beside keeps its folder too: the one written to last counts. Once found it
// is kept while its sessions are there, so a switch of build counts from the
// next start: the watcher is set on it once, and a pick that moved between
// reads would swap the list, and what was told about it, between two builds
#[cfg(windows)]
fn claude_dir(_home: &Path) -> PathBuf {
    static PICKED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    let classic = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("Claude");
    let packaged = std::fs::read_dir(local.join("Claude-Data")).into_iter().flatten().filter_map(Result::ok).map(|e| e.path());
    pick(&mut PICKED.lock().expect("claude folder"), packaged.chain([classic.clone()])).unwrap_or(classic)
}

// the folder picked before while its sessions are there, else the one written
// to last; none found is not kept, so that an app installed later is found
#[cfg(any(windows, test))]
fn pick(picked: &mut Option<PathBuf>, roots: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    if !picked.as_ref().is_some_and(|root| root.join("claude-code-sessions").is_dir()) {
        *picked = latest(roots);
    }
    picked.clone()
}

// of the folders that may hold the app's sessions, the one written to last;
// the first listed when none is newer
#[cfg(any(windows, test))]
fn latest(roots: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    roots
        .into_iter()
        .filter(|root| root.join("claude-code-sessions").is_dir())
        .map(|root| (last_written(&root.join("claude-code-sessions")), root))
        .reduce(|best, next| if next.0 > best.0 { next } else { best })
        .map(|(_, root)| root)
}

// when the app last wrote a session: it writes each anew into its org folder,
// two levels down. Only the files count: a folder's own time changes only when
// an entry in it comes or goes (the two levels above keep the day they were
// made), and a copied folder is new while its files keep their times. A time
// still to come was set by a clock that ran ahead
#[cfg(any(windows, test))]
fn last_written(sessions: &Path) -> Option<SystemTime> {
    let now = SystemTime::now();
    let list = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().filter_map(Result::ok);
    let dirs = |dir: &Path| list(dir).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect::<Vec<_>>();
    let mut newest = None;
    for account in dirs(sessions) {
        for org in dirs(&account) {
            let files = list(&org).filter(|e| e.file_type().is_ok_and(|t| t.is_file()));
            newest = newest.max(files.filter_map(|e| e.metadata().ok()?.modified().ok()).filter(|t| *t <= now).max());
        }
    }
    newest
}

#[cfg(not(windows))]
fn claude_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Claude")
}

pub struct Paths {
    pub app: PathBuf,
    pub recorder: PathBuf,
    pub runs: PathBuf,
    pub pending: PathBuf,
    pub failed: PathBuf,
    pub engines: PathBuf,
    pub projects: PathBuf,
    pub leveldb: PathBuf,
    pub codex: PathBuf,
}

pub fn paths() -> Result<Paths, String> {
    let home = home()?;
    let claude = claude_dir(&home);
    Ok(Paths {
        app: claude.join("claude-code-sessions"),
        recorder: home.join(".claude/routine-recorder"),
        runs: home.join(".claude/routine-recorder/runs"),
        pending: home.join(".claude/routine-recorder/pending"),
        failed: home.join(".claude/routine-recorder/failed"),
        engines: home.join(".claude/sessions"),
        projects: home.join(".claude/projects"),
        leveldb: claude.join("Local Storage/leveldb"),
        // Codex moves its folder where CODEX_HOME says
        codex: std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".codex")),
    })
}

pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

pub fn mtime_ms(meta: &Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// an ISO timestamp as the transcripts write it, in ms like Date.parse
pub fn parse_time(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis() as f64)
}

pub fn kind(line: &Value) -> Option<&str> {
    line.get("type").and_then(Value::as_str).filter(|t| !t.is_empty())
}

pub fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub fn content(line: &Value) -> Option<&Value> {
    line.get("message").and_then(|m| m.get("content"))
}

// the text blocks of an entry, joined by new lines
pub fn text_blocks(line: &Value) -> String {
    match content(line) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// whether an assistant entry hands a subagent's report back
pub fn is_handback(line: Option<&Value>) -> bool {
    let Some(line) = line else { return false };
    kind(line) == Some("assistant")
        && content(line).and_then(Value::as_array).is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_use")
                    && b.get("name").and_then(Value::as_str) == Some("SubagentHandback")
            })
        })
}

// the JSON lines of a text; lines that do not parse (one still being
// written) are left out
pub fn json_lines(text: &str, skip_first: bool) -> Vec<Value> {
    text.split('\n')
        .skip(usize::from(skip_first))
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// the last `length` bytes of a file
pub fn read_tail(path: &Path, size: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(size - length))?;
    let mut buffer = vec![0; length as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

// the first `length` bytes of a file, fewer if it is shorter
pub fn read_head(path: &Path, length: u64) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    File::open(path)?.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

// the first n characters, as String.slice(0, n)
pub fn first_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_claude_folder_written_to_last_counts() {
        let base = std::env::temp_dir().join(format!("pulse-claude-dir-{}", std::process::id()));
        let data = base.join("Claude-Data");
        // the Store build's folder is listed before the download's, and a
        // copy of the download's after it
        let store = data.join("anthropicpbc.claude_fnn82j28hfe8t");
        let download = data.join("claude_pzs8sxrjxfjjc");
        let copy = data.join("claude_pzs8sxrjxfjjc - Copy");
        let classic = base.join("Claude");
        let roots = || [data.join("README.txt"), store.clone(), download.clone(), copy.clone(), classic.clone()];
        // a session written so many hours into the day before, while its
        // folders are made now
        let day = SystemTime::now() - Duration::from_secs(24 * 3600);
        let hours = |n: u64| day + Duration::from_secs(n * 3600);
        let write = |root: &Path, name: &str, at: SystemTime| {
            let org = root.join("claude-code-sessions/account/org");
            std::fs::create_dir_all(&org).expect("dir");
            File::create(org.join(name)).expect("file").set_modified(at).expect("time");
        };

        for dir in [&store, &download, &copy, &classic] {
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(data.join("README.txt"), "").expect("file");
        assert_eq!(latest(roots()), None);

        // sessions kept but none written yet: the first listed, as before
        std::fs::create_dir_all(store.join("claude-code-sessions/account")).expect("dir");
        std::fs::create_dir_all(download.join("claude-code-sessions/account")).expect("dir");
        assert_eq!(latest(roots()), Some(store.clone()));

        write(&download, "local_a.json", hours(1));
        assert_eq!(latest(roots()), Some(download.clone()));
        write(&store, "local_b.json", hours(2));
        assert_eq!(latest(roots()), Some(store.clone()));
        write(&download, "local_c.json", hours(3));
        assert_eq!(latest(roots()), Some(download.clone()));
        // a session dated on by a clock that ran ahead counts for nothing
        write(&store, "local_d.json", SystemTime::now() + Duration::from_secs(30 * 24 * 3600));
        assert_eq!(latest(roots()), Some(download.clone()));
        // a copy's folders are new but its files are not: it does not win
        write(&copy, "local_c.json", hours(3));
        std::fs::create_dir_all(copy.join("claude-code-sessions/account/org/waiting-input")).expect("dir");
        assert_eq!(latest(roots()), Some(download.clone()));
        write(&classic, "local_e.json", hours(4));
        assert_eq!(latest(roots()), Some(classic.clone()));

        // the folder picked is kept while its sessions are there, though
        // another is written to later, and given up when they go
        let mut picked = None;
        assert_eq!(pick(&mut picked, roots()), Some(classic.clone()));
        write(&download, "local_f.json", hours(5));
        assert_eq!(latest(roots()), Some(download.clone()));
        assert_eq!(pick(&mut picked, roots()), Some(classic.clone()));
        std::fs::remove_dir_all(classic.join("claude-code-sessions")).expect("remove");
        assert_eq!(pick(&mut picked, roots()), Some(download.clone()));
        // none found is not kept
        let mut picked = None;
        assert_eq!(pick(&mut picked, [classic.clone()]), None);
        write(&classic, "local_g.json", hours(6));
        assert_eq!(pick(&mut picked, [classic.clone()]), Some(classic.clone()));

        let _ = std::fs::remove_dir_all(&base);
    }
}
