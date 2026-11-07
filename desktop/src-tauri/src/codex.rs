// The Codex app's threads, beside the Claude app's sessions: its state
// database (~/.codex/state_5.sqlite) names each thread, its folder, whether it
// is archived and whether it has a turn not yet read (the CLI marks nothing
// read: what it answered was read in the terminal); the thread's rollout
// (~/.codex/sessions/…/rollout-*.jsonl) says whether a turn runs now, what it
// does and what it answered last. A Codex thread is an item like a chat; its
// id is "codex_" and the thread's UUID. The database is the app's own and may
// change; a failed read is reported as a problem and leaves Codex out.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::inputs::{Inputs, Session};
use crate::rules::limits::WINDOW_MS;
use crate::live::Live;
use crate::runs::Summary;
use crate::said::{status_of, Said};
use crate::util::parse_time;

pub const PREFIX: &str = "codex_";
// a rollout is read in pieces this large: a long turn (megabytes of tool
// output) is never held whole
const CHUNK: u64 = 1024 * 1024;
// a turn that started but wrote no event of its own for this long was cut off
// (the app closed), by the event's own time: not the file's, which on Windows
// stays as it was while Codex holds the rollout open, and which a line not the
// turn's moves (the thread's settings, which Codex writes when it opens the
// thread or its settings change)
const STALE_MS: f64 = 6.0 * 3_600_000.0;

struct Thread {
    id: String,
    title: String,
    cwd: Option<String>,
    // what names its project where the folder's own name would not
    project: Option<String>,
    rollout: PathBuf,
    archived: bool,
    created_at: Option<f64>,
    // when its last turn started (the app also bumps updated_at when it migrates a thread)
    updated_at: Option<f64>,
    unread: bool,
}

// what a rollout's end says
#[derive(Clone, Default)]
struct Turn {
    running: bool,
    started: Option<f64>,
    // when its last turn ended, by the event's own time
    ended: Option<f64>,
    // when the turn last wrote an event, by the event's own time
    last_at: Option<f64>,
    // the last tool call of the running turn, and how many
    tool: Option<String>,
    tools: usize,
    last_message: Option<String>,
    aborted: bool,
}

// per rollout: how far it was read (to its last whole line), when the file was
// made (where the system says) and what that said; a rollout that grew is read
// on from there, one written anew is read again
type Cached = (u64, Option<SystemTime>, Turn);
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Cached>>> = LazyLock::new(Default::default);
// the rollouts the last read looked at, for the check between refreshes
static RECENT: LazyLock<Mutex<BTreeSet<PathBuf>>> = LazyLock::new(Default::default);

// History reads old rollouts on demand, without adding them to the live cache or
// the paths watched between refreshes. A rewritten file must be read again even
// when its size and creation time stayed the same.
#[derive(Clone)]
pub struct HistorySummary {
    pub text: String,
    pub at: Option<f64>,
}

type FileStamp = (u64, Option<SystemTime>, Option<SystemTime>);
type Resting = (FileStamp, Option<HistorySummary>);
static RESTING: LazyLock<Mutex<HashMap<PathBuf, Resting>>> = LazyLock::new(Default::default);
#[cfg(test)]
static HISTORY_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// Windows writes long paths with \\?\ before them, a share's as \\?\UNC\server\share
fn plain(path: &str) -> String {
    if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{share}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

fn first_line(text: &str, n: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    crate::util::first_chars(line, n)
}

fn open(db: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
}

// the person's Codex projects by their folders, the innermost first; an older
// database has none
fn projects(conn: &Connection) -> Vec<(PathBuf, String)> {
    let roots = || -> rusqlite::Result<Vec<(PathBuf, String)>> {
        let mut statement = conn.prepare(
            "SELECT r.path, p.name FROM project_roots r JOIN projects p ON p.id = r.project_id WHERE coalesce(p.name, '') <> ''",
        )?;
        let rows = statement.query_map([], |row| Ok((PathBuf::from(plain(&row.get::<_, String>(0)?)), row.get(1)?)))?;
        rows.collect()
    };
    let mut projects = roots().unwrap_or_default();
    // a root with no path would hold every folder
    projects.retain(|(root, _)| root.components().next().is_some());
    projects.sort_by_key(|(root, _)| std::cmp::Reverse(root.components().count()));
    projects
}

// the folder the system keeps the person's documents in, looked up once: on
// Windows it may have been moved, or be OneDrive's, named in the system's
// language (OneDrive\Dokumente). Only its name is asked for
#[cfg(windows)]
fn documents() -> Option<&'static Path> {
    use windows_sys::Win32::UI::Shell::{SHGetFolderPathW, CSIDL_FLAG_DONT_VERIFY, CSIDL_PERSONAL};
    static FOLDER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
        // MAX_PATH characters
        let mut path = [0u16; 260];
        let result = unsafe { SHGetFolderPathW(std::ptr::null_mut(), (CSIDL_PERSONAL | CSIDL_FLAG_DONT_VERIFY) as i32, std::ptr::null_mut(), 0, path.as_mut_ptr()) };
        let length = path.iter().position(|c| *c == 0).unwrap_or(0);
        (result >= 0 && length > 0).then(|| PathBuf::from(String::from_utf16_lossy(&path[..length])))
    });
    FOLDER.as_deref()
}

#[cfg(not(windows))]
fn documents() -> Option<&'static Path> {
    None
}

// a thread in a Codex project goes by the project's name (Codex leaves
// threads.project_id empty; the folder says it). A chat the app started outside
// any project gets a folder of its own, Documents\Codex\<yyyy-mm-dd>\ and the
// prompt's first words (Documents: the system's, or any so named): it has none,
// even where a project's folder holds that one (the home folder, say)
fn project(cwd: &str, projects: &[(PathBuf, String)], documents: Option<&Path>) -> Option<String> {
    let cwd = Path::new(cwd);
    let mut names = cwd.components().rev().skip(1).map(|c| c.as_os_str().to_string_lossy());
    let (day, codex, folder) = (names.next().unwrap_or_default(), names.next().unwrap_or_default(), names.next().unwrap_or_default());
    let date = day.len() == 10 && day.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    // a folder by its parts, whatever their case
    let parts = |path: &Path| path.components().map(|c| c.as_os_str().to_ascii_lowercase()).collect::<Vec<_>>();
    let above = cwd.ancestors().nth(3).is_some_and(|above| documents.is_some_and(|documents| parts(above) == parts(documents)));
    let chat = date && codex.eq_ignore_ascii_case("Codex") && (above || folder.eq_ignore_ascii_case("Documents"));
    match projects.iter().find(|(root, _)| cwd.starts_with(root) && (!chat || root.starts_with(cwd))) {
        Some((_, name)) => Some(name.clone()),
        None => chat.then(String::new),
    }
}

fn threads(db: &Path) -> rusqlite::Result<Vec<Thread>> {
    let conn = open(db)?;
    let (projects, documents) = (projects(&conn), documents());
    // the person's own threads, started in the Codex app or CLI: not a
    // subagent's or a review's, nor one another tool drives (those have no source);
    // old ones too, as a parked one stays and the history lists them all
    let mut statement = conn.prepare(
        "SELECT t.id, t.name, t.title, t.cwd, t.rollout_path, t.archived, t.created_at_ms, coalesce(t.recency_at_ms, t.updated_at_ms),
                r.first_unread_turn IS NOT NULL AND coalesce(t.originator, '') <> 'codex-tui'
         FROM threads t LEFT JOIN thread_read_receipts r ON r.thread_id = t.id
         WHERE t.thread_source = 'user'",
    )?;
    let rows = statement.query_map([], |row| {
        let name: Option<String> = row.get(1)?;
        let title: Option<String> = row.get(2)?;
        let cwd = row.get::<_, Option<String>>(3)?.map(|c| plain(&c));
        let rollout: String = row.get(4)?;
        Ok(Thread {
            id: row.get(0)?,
            title: name.filter(|n| !n.trim().is_empty()).or_else(|| title.map(|t| first_line(&t, 80))).unwrap_or_default(),
            project: cwd.as_deref().and_then(|cwd| project(cwd, &projects, documents)),
            cwd,
            rollout: PathBuf::from(plain(&rollout)),
            archived: row.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
            created_at: row.get::<_, Option<i64>>(6)?.map(|v| v as f64),
            updated_at: row.get::<_, Option<i64>>(7)?.map(|v| v as f64),
            unread: row.get(8)?,
        })
    })?;
    rows.collect()
}

fn time_of(line: &Value) -> Option<f64> {
    line.get("timestamp").and_then(Value::as_str).and_then(parse_time)
}

fn apply(turn: &mut Turn, line: &Value) {
    let Some(payload) = line.get("payload") else { return };
    if line.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let kind = payload.get("type").and_then(Value::as_str);
    match kind {
        Some("task_started") => {
            *turn = Turn { running: true, started: time_of(line), last_message: turn.last_message.take(), ..Default::default() };
        }
        Some("task_complete") => {
            turn.running = false;
            turn.aborted = false;
            turn.ended = time_of(line);
            if let Some(text) = answer(payload) {
                turn.last_message = Some(text.to_string());
            }
        }
        Some("turn_aborted") => {
            turn.running = false;
            turn.aborted = true;
            turn.ended = time_of(line);
        }
        Some("item_completed") if turn.running => {
            let item = payload.get("item");
            let tool = match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                Some("CommandExecution") => Some("Shell".to_string()),
                Some("FileChange") => Some("Edit".to_string()),
                Some("McpToolCall") => item.and_then(|i| i.get("tool")).and_then(Value::as_str).map(str::to_string).or(Some("MCP".into())),
                Some("CollabAgentToolCall" | "SubAgentActivity") => Some("Agent".to_string()),
                Some("ImageView") => Some("View image".to_string()),
                _ => None,
            };
            if let Some(tool) = tool {
                turn.tool = Some(tool);
                turn.tools += 1;
            }
        }
        _ => {}
    }
    // after the match: a task_started starts the turn anew. The thread's
    // settings are not the turn's: Codex writes them when it opens the thread
    // or its settings change, hours after the turn
    if kind != Some("thread_settings_applied") {
        turn.last_at = time_of(line).or(turn.last_at);
    }
}

// what a task_complete answered, if anything
fn answer(payload: &Value) -> Option<&str> {
    payload.get("last_agent_message").and_then(Value::as_str).filter(|t| !t.trim().is_empty())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// a line where a turn ended with an answer
fn is_answer(line: &[u8]) -> bool {
    contains(line, b"task_complete")
        && serde_json::from_slice::<Value>(line).is_ok_and(|line| {
            line.get("type").and_then(Value::as_str) == Some("event_msg")
                && line.get("payload").filter(|p| p.get("type").and_then(Value::as_str) == Some("task_complete")).and_then(answer).is_some()
        })
}

// where the last answer's line starts, else the file's start: read from there,
// a rollout ends as read whole (a task_started resets all but the answer before;
// before that answer nothing counts), however long its last turn ran. Read
// backwards in chunks; a line cut by a chunk's start is carried to the next.
fn last_answer(path: &Path, size: u64) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    let (mut pos, mut buffer, mut carry) = (size, Vec::new(), Vec::new());
    // the end of the file may be a line still being written
    let mut skip_last = true;
    while pos > 0 {
        let start = pos.saturating_sub(CHUNK);
        buffer.clear();
        buffer.resize((pos - start) as usize, 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer)?;
        buffer.extend_from_slice(&carry);
        pos = start;
        // the lines that begin in this chunk: after its first line end, or all at the file's start
        let first = match buffer.iter().position(|&b| b == b'\n') {
            _ if start == 0 => 0,
            Some(i) => i + 1,
            None => {
                std::mem::swap(&mut carry, &mut buffer);
                continue;
            }
        };
        let lines = &buffer[first..];
        let mut offset = lines.len();
        for (n, line) in lines.rsplit(|&b| b == b'\n').enumerate() {
            offset -= line.len();
            if !(n == 0 && skip_last) && is_answer(line) {
                return Ok(start + (first + offset) as u64);
            }
            offset = offset.saturating_sub(1);
        }
        skip_last = false;
        carry.clear();
        carry.extend_from_slice(&buffer[..first]);
    }
    Ok(0)
}

// apply the whole lines from `from` on, in chunks; returns where the last whole
// line ends (one still being written is left for later)
fn read_on(path: &Path, from: u64, turn: &mut Turn) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut reader = BufReader::with_capacity(CHUNK as usize, file);
    let (mut read, mut line) = (from, Vec::new());
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Ok(read);
        }
        read += n as u64;
        // only events count; the rest (tool output, reasoning) is not parsed
        if contains(&line, b"event_msg") {
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                apply(turn, &value);
            }
        }
    }
}

// whether a line starts at `at`: where a rollout written anew in place was read
// to is mostly in the middle of one
fn line_starts(path: &Path, at: u64) -> bool {
    if at == 0 {
        return true;
    }
    let mut before = [0];
    let read = File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(at - 1))?;
        file.read_exact(&mut before)
    });
    read.is_ok() && before == *b"\n"
}

fn turn_of(rollout: &Path) -> Option<Turn> {
    let meta = std::fs::metadata(rollout).ok()?;
    let (size, made) = (meta.len(), meta.created().ok());
    // the file read before, not a new one renamed over it (as when Codex
    // migrates a thread); where the system can't say, it is taken as the same.
    // On NTFS a file put under a name deleted or renamed away seconds before
    // gets the old one's creation time: one of the same size passes for it
    let known = CACHE.lock().expect("codex cache").get(rollout).cloned().filter(|(_, at, _)| match (at, made) {
        (Some(at), Some(made)) => *at == made,
        _ => true,
    });
    let (from, mut turn) = match known {
        Some((read, _, turn)) if read == size => return Some(turn),
        // it grew: read on from where it was left
        Some((read, _, turn)) if read < size && line_starts(rollout, read) => (read, turn),
        // new or rewritten
        _ => (last_answer(rollout, size).ok()?, Turn::default()),
    };
    let read = read_on(rollout, from, &mut turn).ok()?;
    CACHE.lock().expect("codex cache").insert(rollout.to_path_buf(), (read, made, turn.clone()));
    Some(turn)
}

// Codex compresses a rollout idle for some days to rollout-….jsonl.zst and
// keeps the plain name in its database; it unpacks it again to go on with it
fn packed(rollout: &Path) -> PathBuf {
    let mut name = rollout.as_os_str().to_owned();
    name.push(".zst");
    PathBuf::from(name)
}

// a compressed rollout is read whole, in order (a stream is not read from its
// end), and once: it does not change, so the same size and creation time say
// it is the one read before. A line it can't unpack ends it
fn unpacked(zst: &Path) -> Option<Turn> {
    let meta = std::fs::metadata(zst).ok()?;
    let (size, made) = (meta.len(), meta.created().ok());
    if let Some((_, _, turn)) = CACHE.lock().expect("codex cache").get(zst).filter(|(read, at, _)| *read == size && *at == made) {
        return Some(turn.clone());
    }
    let turn = decode(zst)?;
    CACHE.lock().expect("codex cache").insert(zst.to_path_buf(), (size, made, turn.clone()));
    Some(turn)
}

fn decode(zst: &Path) -> Option<Turn> {
    let decoder = ruzstd::decoding::StreamingDecoder::new(File::open(zst).ok()?).ok()?;
    let mut reader = BufReader::with_capacity(CHUNK as usize, decoder);
    let (mut turn, mut line) = (Turn::default(), Vec::new());
    while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        if contains(&line, b"event_msg") {
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                apply(&mut turn, &value);
            }
        }
        line.clear();
    }
    Some(turn)
}

// The same sentence for a live or resting thread: an interrupted turn must not
// reuse the answer from the turn before it.
fn told(turn: &Turn) -> (Option<Said>, String) {
    let message = turn.last_message.as_deref().unwrap_or("");
    let said = if turn.aborted { None } else { status_of(message) };
    let detail = match (&said, turn.aborted) {
        (_, true) => "Interrupted".to_string(),
        (Some(s), _) if !s.text.is_empty() => s.text.clone(),
        _ => conclusion(message),
    };
    (said, detail)
}

pub fn said_at_rest(rollout: &Path) -> Option<HistorySummary> {
    let (path, meta, compressed) = match std::fs::metadata(rollout) {
        Ok(meta) => (rollout.to_path_buf(), meta, false),
        Err(_) => {
            let zst = packed(rollout);
            let meta = std::fs::metadata(&zst).ok()?;
            (zst, meta, true)
        }
    };
    let stamp = (meta.len(), meta.created().ok(), meta.modified().ok());
    if let Some((_, summary)) = RESTING.lock().expect("codex history cache").get(&path).filter(|(at, _)| *at == stamp) {
        return summary.clone();
    }
    #[cfg(test)]
    HISTORY_READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let turn = if compressed {
        decode(&path)
    } else {
        last_answer(&path, meta.len()).ok().and_then(|from| {
            let mut turn = Turn::default();
            read_on(&path, from, &mut turn).ok().map(|_| turn)
        })
    };
    let summary = turn.map(|turn| HistorySummary { text: told(&turn).1, at: turn.ended.or(turn.last_at) });
    RESTING.lock().expect("codex history cache").insert(path, (stamp, summary.clone()));
    summary
}

// an answer that ends in a question, its emphasis left aside: "**Go on?**", "_Go on?_"
fn ends_in_question(message: &str) -> bool {
    message.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_')).ends_with('?')
}

// the answer's last paragraph: where a turn says what came of it
fn conclusion(text: &str) -> String {
    // of its prose: a code block and a directive's fences (:::writing{…} … :::)
    // are blanked, its text kept, so the paragraphs stay apart. A code block's
    // fence is three backticks or tildes or more (```ls``` is code in a line);
    // as many of the same and nothing else close it, so a longer fence holds one
    let mut block: Option<(char, usize)> = None;
    let prose = text
        .lines()
        .map(|line| {
            let start = line.trim_start();
            let mark = start.chars().next().filter(|c| *c == '`' || *c == '~');
            let marks = mark.map_or(0, |m| start.len() - start.trim_start_matches(m).len());
            let tag = &start[marks..];
            match block {
                Some((m, n)) => {
                    if mark == Some(m) && marks >= n && tag.trim().is_empty() {
                        block = None;
                    }
                    ""
                }
                None if marks >= 3 && !(mark == Some('`') && tag.contains('`')) => {
                    block = mark.map(|m| (m, marks));
                    ""
                }
                None if start.starts_with(":::") => "",
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let paragraph = prose.split("\n\n").map(str::trim).filter(|p| !p.is_empty() && !crate::said::bare(p).starts_with("STATUS:")).last().unwrap_or("");
    // as plain text: no bold, a link by its text, no code marks. Of a long one
    // only the start, far more than is shown, is looked through (a link that
    // never closes is looked for to the end from each bracket), and a link the
    // cut goes through is left out
    let head = crate::util::first_chars(paragraph, 4000);
    let cut = head.len() < paragraph.len();
    let text = link_text(&head, cut).replace('`', "");
    // a list, each line an item, by its items: "- one\n- two" as "one; two",
    // an item that ends in a mark of its own gets none: "- One.\n- Two." as "One. Two."
    // (a notification puts " · " between the project and the reason)
    let items: Option<Vec<&str>> = text.lines().map(|row| row.trim_start().strip_prefix(['-', '*', '+']).filter(|item| item.starts_with([' ', '\t']))).collect();
    let text = match items {
        Some(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let item = item.trim_end();
                if i + 1 < items.len() && !item.ends_with(['.', '!', '?', ',', ':', ';']) { format!("{item};") } else { item.to_string() }
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => text,
    };
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = crate::util::first_chars(&line, 200);
    if short.len() < line.len() || cut { format!("{short}…") } else { short }
}

// [text](target) as its text, an image's ![text](target) too; the target's own
// brackets are matched (a Windows path has "(x86)"). Code in a line is no link,
// nor bold: the prose and a link's text lose their "**", code keeps its own
// (`2**10`). Of a text cut short, a link the cut went through is left out, not
// shown as written
fn link_text(text: &str, cut: bool) -> String {
    let (mut out, mut rest) = (String::new(), text);
    while let Some(open) = rest.find(['[', '`']) {
        if rest[open..].starts_with('`') {
            // as it is, to the mark that closes it
            let end = rest[open + 1..].find('`').map_or(open + 1, |close| open + close + 2);
            out += &rest[..open].replace("**", "");
            out += &rest[open..end];
            rest = &rest[end..];
            continue;
        }
        let after = &rest[open + 1..];
        let link = after.find(']').and_then(|close| {
            let target = after[close + 1..].strip_prefix('(')?;
            let mut depth = 0;
            let end = target.find(|c| {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                depth < 0
            })?;
            Some((&after[..close], &target[end + 1..]))
        });
        match link {
            Some((label, next)) => {
                out += &rest[..open].strip_suffix('!').unwrap_or(&rest[..open]).replace("**", "");
                // its text read the same way (it may be code)
                out += &link_text(label, false);
                rest = next;
            }
            // its text, or its target, runs on to the end
            None if cut && after.split_once(']').is_none_or(|(_, next)| next.is_empty() || next.starts_with('(')) => {
                return out + &rest[..open].strip_suffix('!').unwrap_or(&rest[..open]).replace("**", "");
            }
            None => {
                out += &rest[..=open].replace("**", "");
                rest = after;
            }
        }
    }
    out + &rest.replace("**", "")
}

pub fn read(codex: &Path, inputs: &mut Inputs) {
    let db = codex.join("state_5.sqlite");
    if !db.exists() {
        return;
    }
    let now = inputs.now;
    let list = match threads(&db) {
        Ok(list) => list,
        Err(error) => {
            inputs.problems.push(format!("Codex threads unreadable: {error}"));
            return;
        }
    };
    let mut seen = BTreeSet::new();
    let mut resting = BTreeSet::new();
    for t in list {
        let id = format!("{PREFIX}{}", t.id);
        // only a recent thread's rollout is read; older ones are listed for the
        // history and what is parked, and the panel leaves the rest out
        let recent = !t.archived && t.updated_at.is_some_and(|at| at > now - WINDOW_MS);
        if !recent {
            resting.insert(t.rollout.clone());
            resting.insert(packed(&t.rollout));
        }
        let turn = if recent {
            seen.insert(t.rollout.clone());
            turn_of(&t.rollout).or_else(|| {
                // compressed: its .zst is read instead, and kept in the cache
                // and looked at between refreshes as the rollout is
                let zst = packed(&t.rollout);
                seen.insert(zst.clone());
                unpacked(&zst)
            })
        } else {
            None
        };
        let turn = turn.unwrap_or_default();
        let written = if t.updated_at.is_none() {
            std::fs::metadata(&t.rollout).map(|m| crate::util::mtime_ms(&m)).unwrap_or(0.0)
        } else {
            0.0
        };
        let running = turn.running && now - turn.last_at.unwrap_or(0.0) < STALE_MS;
        let message = turn.last_message.as_deref().unwrap_or("");
        let (said, detail) = told(&turn);
        // without one, an answer that ends in a question asks the person something
        let asks = said.is_none() && !turn.aborted && ends_in_question(message);
        if running {
            inputs.running.insert(id.clone());
            inputs.in_turn.insert(id.clone());
            inputs.live.insert(id.clone(), Live::of_codex(turn.started, turn.tool.clone(), turn.tools));
        }
        if let Some(said) = said {
            inputs.said.insert(id.clone(), Said { state: said.state, text: said.text });
        }
        if t.unread {
            inputs.unread.insert(id.clone());
        }
        inputs.sessions.push(Session {
            id,
            title: Some(t.title).filter(|t| !t.is_empty()),
            cwd: t.cwd,
            // what names the project before the cwd does: a Codex project's
            // name, or none for a chat's own folder
            origin_cwd: t.project,
            archived: t.archived,
            rollout: (!recent).then_some(t.rollout),
            created_at: t.created_at,
            // from when its last turn ended (one running or cut off: its last
            // event); a thread not read, from when its last turn started
            last_activity: turn.ended.or(turn.last_at).or(t.updated_at).or(Some(written)),
            summary: Summary { category: None, needs_action: if asks { detail.clone() } else { String::new() }, detail },
            ..Default::default()
        });
    }
    // a thread no longer read (archived, quiet too long) leaves the cache
    CACHE.lock().expect("codex cache").retain(|path, _| seen.contains(path));
    RESTING.lock().expect("codex history cache").retain(|path, _| resting.contains(path));
    *RECENT.lock().expect("codex recent") = seen;
}

// the threads as a refresh shows them, hashed: a new thread or turn, a turn
// read, a rename or an archive changes it
fn state(db: &Path) -> rusqlite::Result<u64> {
    let conn = open(db)?;
    let mut statement = conn.prepare(
        "SELECT json_array(t.id, t.name, t.title, t.archived, t.recency_at_ms, t.updated_at_ms, r.first_unread_turn)
         FROM threads t LEFT JOIN thread_read_receipts r ON r.thread_id = t.id
         WHERE t.thread_source = 'user'",
    )?;
    let mut hasher = DefaultHasher::new();
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        row?.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

// what the check between refreshes compares (watch.rs): the size of each
// rollout the last read looked at (the watcher misses what Codex appends to
// one it holds open, on Windows at least) and the threads in the database (a
// turn read or a rename writes no rollout). None while the database can't be
// read; without Codex it stays the same.
pub fn token(codex: &Path) -> Option<u64> {
    let rollouts = RECENT.lock().expect("codex recent").clone();
    let mut hasher = DefaultHasher::new();
    for rollout in rollouts {
        let size = std::fs::metadata(&rollout).ok().map(|m| m.len());
        (rollout, size).hash(&mut hasher);
    }
    let db = codex.join("state_5.sqlite");
    let threads = if db.exists() { Some(state(&db).ok()?) } else { None };
    threads.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn event(kind: &str, extra: Value) -> Value {
        let mut payload = serde_json::json!({ "type": kind });
        payload.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
        serde_json::json!({ "timestamp": "2026-01-15T10:00:00.000Z", "type": "event_msg", "payload": payload })
    }

    #[test]
    fn a_turn_runs_until_it_completes() {
        let mut turn = Turn::default();
        apply(&mut turn, &event("task_started", serde_json::json!({})));
        apply(&mut turn, &event("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } })));
        assert!(turn.running);
        assert_eq!((turn.tool.as_deref(), turn.tools), (Some("Shell"), 1));
        assert!(turn.started.is_some());
        apply(&mut turn, &event("task_complete", serde_json::json!({ "last_agent_message": "Done.\n\nSTATUS: worked | Tests pass" })));
        assert!(!turn.running);
        let said = status_of(turn.last_message.as_deref().unwrap_or("")).expect("status");
        assert_eq!((said.state.as_str(), said.text.as_str()), ("worked", "Tests pass"));
    }

    #[test]
    fn an_aborted_turn_keeps_the_answer_before() {
        let mut turn = Turn::default();
        apply(&mut turn, &event("task_complete", serde_json::json!({ "last_agent_message": "First answer" })));
        apply(&mut turn, &event("task_started", serde_json::json!({})));
        apply(&mut turn, &event("turn_aborted", serde_json::json!({ "reason": "interrupted" })));
        assert!(!turn.running && turn.aborted);
        assert_eq!(turn.last_message.as_deref(), Some("First answer"));
    }

    #[test]
    fn a_long_turn_runs_and_a_grown_rollout_is_read_on() {
        use std::io::Write;
        let _one = one_at_a_time();
        let path = std::env::temp_dir().join(format!("pulse-rollout-{}.jsonl", std::process::id()));
        let line = |v: Value| format!("{v}\n");
        let mut text = line(event("task_complete", serde_json::json!({ "last_agent_message": "Before" })));
        text += &line(event("task_started", serde_json::json!({})));
        // more tool output than one chunk
        let output = "x".repeat(1000);
        for _ in 0..3000 {
            text += &line(event("item_completed", serde_json::json!({ "item": { "type": "CommandExecution", "output": output } })));
        }
        std::fs::write(&path, &text).expect("write");
        let turn = turn_of(&path).expect("turn");
        assert!(turn.running && turn.started.is_some());
        assert_eq!((turn.tools, turn.last_message.as_deref()), (3000, Some("Before")));
        // a line still being written is left for later
        let done = line(event("task_complete", serde_json::json!({ "last_agent_message": "After" })));
        let (head, rest) = done.split_at(20);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(head.as_bytes()).expect("append");
        assert!(turn_of(&path).expect("turn").running);
        file.write_all(rest.as_bytes()).expect("append");
        let turn = turn_of(&path).expect("turn");
        assert!(!turn.running);
        assert_eq!(turn.last_message.as_deref(), Some("After"));
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    // `count` tool calls, each with `size` bytes of output that mentions a task_complete
    fn write_tools(file: &mut impl std::io::Write, count: usize, size: usize) {
        let output = format!("task_complete {}", "x".repeat(size));
        let line = event("item_completed", serde_json::json!({ "item": { "type": "CommandExecution", "output": output } }));
        for _ in 0..count {
            writeln!(file, "{line}").expect("write");
        }
    }

    #[test]
    fn a_turn_longer_than_any_look_at_the_end_still_runs() {
        use std::io::Write;
        let _one = one_at_a_time();
        let path = std::env::temp_dir().join(format!("pulse-rollout-cold-{}.jsonl", std::process::id()));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
        writeln!(file, "{}", event("task_complete", serde_json::json!({ "last_agent_message": "Before" }))).expect("write");
        writeln!(file, "{}", event("task_started", serde_json::json!({}))).expect("write");
        // 34.5 MiB of tool output, lines across the chunks
        write_tools(&mut file, 23, 3 * 512 * 1024);
        drop(file);
        let turn = turn_of(&path).expect("turn");
        assert!(turn.running && turn.started.is_some());
        assert_eq!((turn.tools, turn.last_message.as_deref()), (23, Some("Before")));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_running_turn_is_read_on_however_much_was_added() {
        use std::io::Write;
        let _one = one_at_a_time();
        let path = std::env::temp_dir().join(format!("pulse-rollout-warm-{}.jsonl", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create");
        writeln!(file, "{}", event("task_complete", serde_json::json!({ "last_agent_message": "Before" }))).expect("write");
        writeln!(file, "{}", event("task_started", serde_json::json!({}))).expect("write");
        write_tools(&mut file, 2, 100);
        assert_eq!(turn_of(&path).expect("turn").tools, 2);
        // 5 MiB more
        write_tools(&mut file, 5, 1024 * 1024);
        let turn = turn_of(&path).expect("turn");
        assert!(turn.running);
        assert_eq!((turn.tools, turn.last_message.as_deref()), (7, Some("Before")));
        writeln!(file, "{}", event("task_complete", serde_json::json!({ "last_agent_message": "After" }))).expect("write");
        let turn = turn_of(&path).expect("turn");
        assert!(!turn.running);
        assert_eq!(turn.last_message.as_deref(), Some("After"));
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_conclusion_is_the_last_paragraph() {
        assert_eq!(conclusion("Looked at it.\n\nShould I merge\n it?\n\nSTATUS: waiting | x"), "Should I merge it?");
        // nor one with no sentence as a comment, a reference or in bold
        for status in ["<!-- STATUS: worked -->", "[//]: # (STATUS: worked)", "**STATUS: worked**"] {
            assert_eq!(conclusion(&format!("Tests pass.\n\n{status}")), "Tests pass.", "{status}");
        }
        // what Codex writes around its prose: a code block at the end, a
        // drafted text in a directive, links to what it made
        let fence = "Installed the package.\n\nThen restart the app and check:\n\n```powershell\nnpm.cmd --version\n```";
        assert_eq!(conclusion(fence), "Then restart the app and check:");
        let card = "Here is a draft.\n\n:::writing{variant=\"standard\" id=\"10001\"}\nWater the plants\nevery other day.\n:::";
        assert_eq!(conclusion(card), "Water the plants every other day.");
        let links = "Saved the notes as a [Markdown summary](C:/Users/me/Documents/Codex/2026-01-15/new-chat/outputs/summary.md).";
        assert_eq!(conclusion(links), "Saved the notes as a Markdown summary.");
        assert_eq!(conclusion("Done.\n\n[Backup](C:/x/before.zip) · [Updated source](C:/Program Files (x86)/src) [not a link]"), "Backup · Updated source [not a link]");
        // a blank line in a code block; only code; code in a line
        assert_eq!(conclusion("Fixed it.\n\n```rust\nfn a() {}\n\nfn b() {}\n```"), "Fixed it.");
        assert_eq!(conclusion("```sh\nls\n```"), "");
        assert_eq!(conclusion("All there.\n\n```ls -la``` lists them."), "ls -la lists them.");
        // a longer fence holds a shorter one; tildes fence too, and only tildes
        // close them; a fence with a tag after it closes nothing
        assert_eq!(conclusion("Done.\n\n````markdown\n```sh\nls\n```\nThen commit.\n````"), "Done.");
        assert_eq!(conclusion("Run this:\n\n~~~sh\n```\nrm -rf build\n~~~"), "Run this:");
        assert_eq!(conclusion("Built.\n\n```sh\nnpm i\n```sh\nnpm test\n```"), "Built.");
        // an image by its text; code in a line is no link, though a link's text may be code
        assert_eq!(conclusion("Here they are.\n\n![Chart 1](C:/x/c1.png) ![Map](C:/x/m.png)"), "Chart 1 Map");
        assert_eq!(conclusion("Call `handlers[0](event)` in [`codex.rs`](C:/x/codex.rs)."), "Call handlers[0](event) in codex.rs.");
        // bold marks go from the prose and a link's text, never from code; a '!'
        // before a bold mark is no image's
        assert_eq!(conclusion("Done.\n\nUpdated `src/**/*.rs` and `2**10` too."), "Updated src/**/*.rs and 2**10 too.");
        assert_eq!(conclusion("Ran `ls **/*.rs` in **src**."), "Ran ls **/*.rs in src.");
        assert_eq!(conclusion("**Done!**[report](C:/x/r.md)"), "Done!report");
        assert_eq!(conclusion("Saved [**the brief**](C:/x/b.md) and [`**kwargs`](C:/x/a.py) in **src**."), "Saved the brief and **kwargs in src.");
        assert_eq!(conclusion("**Two** [of three] left."), "Two [of three] left.");
        // a list by its items; one with a line of prose in it, or a minus sign, as it is
        let list = "Saved them:\n\n- [Chart 1](C:/x/c1.png)\n- [Map image](C:/x/m.png)\n* **Notes** in `notes.md`";
        assert_eq!(conclusion(list), "Chart 1; Map image; Notes in notes.md");
        // '+' marks one too, a tab may follow the mark, and an item may be indented
        assert_eq!(conclusion("Changed:\n\n+ the reader\n  -\tits test"), "the reader; its test");
        // items that end a sentence run on as sentences; the space after an item is no part of it
        assert_eq!(conclusion("Done.\n\n- The reader is fixed.\n- Its test passes!\n- Shall I merge it?\n- CI still runs"), "The reader is fixed. Its test passes! Shall I merge it? CI still runs");
        assert_eq!(conclusion("Saved:\n\n- one.  \n- two\t\n- three"), "one. two; three");
        assert_eq!(conclusion("Bought:\n\n- apples,\n- pears:\n- plums;\n- figs"), "apples, pears: plums; figs");
        assert_eq!(conclusion("Saved them:\n- one\n- two"), "Saved them: - one - two");
        assert_eq!(conclusion("- one\n-5 °C at night"), "- one -5 °C at night");
        assert_eq!(plain(r"\\?\C:\Users\me\work"), r"C:\Users\me\work");
        assert_eq!(plain(r"\\?\UNC\server\share\me\.codex"), r"\\server\share\me\.codex");
    }

    // of a long paragraph only the start is looked through, and a link the cut
    // goes through is left out, not shown as written
    #[test]
    fn a_cut_leaves_out_the_link_it_goes_through() {
        let long = "very/long/".repeat(450);
        assert_eq!(conclusion(&format!("See [the report](C:/{long}report.html).")), "See…");
        // an image's '!' with it; a cut in its text, or just after it, too
        assert_eq!(conclusion(&format!("Here it is: ![chart](data:image/png;base64,{long})")), "Here it is:…");
        // a bold mark between the '!' and the link makes it no image: the '!' stays
        assert_eq!(conclusion(&format!("**Done!**[the report](C:/{long}report.html)")), "Done!…");
        assert_eq!(conclusion(&format!("Saved [{long}](C:/x/brief.md)")), "Saved…");
        assert_eq!(conclusion(&format!("Saved [{}](C:/x/brief.md)", "x".repeat(3992))), "Saved…");
        // a link before the cut stays, and a bracket that is no link
        assert_eq!(conclusion(&format!("[Backup](C:/x/before.zip) [1/2] [the report](C:/{long})")), "Backup [1/2]…");
        // one that never closes, in a paragraph not cut, is shown as written
        assert_eq!(conclusion("See [the report](C:/x (draft).html"), "See [the report](C:/x (draft).html");
    }

    fn event_at(kind: &str, extra: Value, at: f64) -> Value {
        let mut line = event(kind, extra);
        let time = chrono::DateTime::from_timestamp_millis(at as i64).expect("time");
        line["timestamp"] = time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true).into();
        line
    }

    // the cache and the rollouts the last read looked at are the process's: the
    // tests that rely on them take turns
    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        static ONE: Mutex<()> = Mutex::new(());
        ONE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // a Codex folder of its own: its database, as much of it as is read
    fn codex_home(tag: &str) -> (PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!("pulse-codex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let db = Connection::open(dir.join("state_5.sqlite")).expect("db");
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())).expect("wal");
        db.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL, title TEXT NOT NULL,
                 archived INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER, updated_at_ms INTEGER, thread_source TEXT,
                 recency_at_ms INTEGER NOT NULL DEFAULT 0, name TEXT, originator TEXT);
             CREATE TABLE thread_read_receipts (thread_id TEXT PRIMARY KEY NOT NULL, first_unread_turn TEXT,
                 revision TEXT NOT NULL, last_published_turn TEXT);
             CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL);
             CREATE TABLE project_roots (project_id TEXT NOT NULL, position INTEGER NOT NULL, path TEXT NOT NULL);",
        )
        .expect("schema");
        (dir, db)
    }

    // a thread the person started, last active at `at`
    fn add_thread(db: &Connection, id: &str, rollout: &Path, at: f64, source: &str) {
        db.execute(
            "INSERT INTO threads (id, rollout_path, cwd, title, created_at_ms, updated_at_ms, recency_at_ms, thread_source, name)
             VALUES (?1, ?2, '/work', 'A task', ?3, ?3, ?3, ?4, 'A task')",
            rusqlite::params![id, rollout.to_string_lossy(), at as i64, source],
        )
        .expect("thread");
    }

    fn write_lines(path: &Path, lines: &[Value]) {
        std::fs::write(path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).expect("write");
    }

    // when a read says the thread was last active: its item's since
    fn since(inputs: &Inputs, id: &str) -> Option<f64> {
        inputs.sessions.iter().find(|s| s.id == format!("{PREFIX}{id}")).and_then(|s| s.last_activity)
    }

    #[test]
    fn a_turn_runs_while_its_events_are_recent_whatever_the_file_time() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms().floor();
        let hours_ago = |hours: f64| now - hours * 3_600_000.0;
        let (dir, db) = codex_home("live");
        // a turn started at `started` whose last tool call was at `last` (none
        // yet: the task_started is the last line), in a rollout whose file time
        // stayed 7 h back (as on Windows while Codex holds it open)
        let rollout = |name: &str, started: f64, last: Option<f64>| {
            let path = dir.join(name);
            let mut lines = vec![
                event_at("task_complete", serde_json::json!({ "last_agent_message": "Before" }), started - 30_000.0),
                event_at("task_started", serde_json::json!({}), started),
            ];
            lines.extend(last.map(|at| event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), at)));
            std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).expect("write");
            let file = std::fs::OpenOptions::new().write(true).open(&path).expect("open");
            file.set_modified(UNIX_EPOCH + Duration::from_millis(hours_ago(7.0) as u64)).expect("file time");
            path
        };
        let busy = rollout("busy.jsonl", now - 90_000.0, Some(now - 60_000.0));
        add_thread(&db, "busy", &busy, now - 90_000.0, "user");
        add_thread(&db, "long", &rollout("long.jsonl", hours_ago(7.0), Some(now - 60_000.0)), hours_ago(7.0), "user");
        add_thread(&db, "new", &rollout("new.jsonl", now - 10_000.0, None), now - 10_000.0, "user");
        add_thread(&db, "cut", &rollout("cut.jsonl", hours_ago(7.0), Some(hours_ago(7.0))), hours_ago(7.0), "user");
        // cut off as well, then the thread's settings a minute ago (Codex opened
        // it again): not the turn's, though the file's time is now theirs
        let reopened = dir.join("reopened.jsonl");
        write_lines(&reopened, &[
            event_at("task_started", serde_json::json!({}), hours_ago(7.5)),
            event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), hours_ago(7.0)),
            event_at("thread_settings_applied", serde_json::json!({ "thread_id": "reopened", "thread_settings": {} }), now - 60_000.0),
        ]);
        add_thread(&db, "reopened", &reopened, hours_ago(7.5), "user");
        assert!(now - crate::util::mtime_ms(&std::fs::metadata(&busy).expect("meta")) > STALE_MS);
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert!(inputs.problems.is_empty(), "{:?}", inputs.problems);
        assert!(inputs.running.contains("codex_busy") && inputs.live.contains_key("codex_busy"));
        // by its last event, not its start
        assert!(inputs.running.contains("codex_long"), "a turn longer than STALE_MS, its last event a minute ago");
        // a task_started's time counts, though it starts the turn anew
        assert!(inputs.running.contains("codex_new"), "a turn whose task_started is the last line");
        // its last event is older than STALE_MS: it was cut off
        assert!(!inputs.running.contains("codex_cut"));
        assert!(!inputs.running.contains("codex_reopened"), "a cut-off turn, then the thread's settings a minute ago");
        assert_eq!(since(&inputs, "reopened"), Some(hours_ago(7.0)));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a Codex item counts from when its last turn ended, as a chat from the end
    // of its last activity: not from the turn's start, the database's time
    #[test]
    fn an_answer_counts_from_the_end_of_its_turn() {
        let _one = one_at_a_time();
        let (now, min) = (crate::util::now_ms().floor(), 60_000.0);
        let (dir, db) = codex_home("ended");
        // a 20 min turn that answered a minute ago, after more tool output than
        // one chunk: only the end is read, from the answer on
        let output = "x".repeat(1000);
        let mut lines = vec![event_at("task_started", serde_json::json!({}), now - 21.0 * min)];
        lines.extend((0..1100).map(|n| event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution", "output": output } }), now - 20.0 * min + n as f64)));
        lines.push(event_at("task_complete", serde_json::json!({ "last_agent_message": "Should I merge it?" }), now - min));
        let answered = dir.join("answered.jsonl");
        write_lines(&answered, &lines);
        assert!(std::fs::metadata(&answered).expect("meta").len() > CHUNK);
        add_thread(&db, "answered", &answered, now - 21.0 * min, "user");
        // interrupted 5 min ago; the call it cut short came back after
        let interrupted = dir.join("interrupted.jsonl");
        write_lines(&interrupted, &[
            event_at("task_complete", serde_json::json!({ "last_agent_message": "Before" }), now - 30.0 * min),
            event_at("task_started", serde_json::json!({}), now - 25.0 * min),
            event_at("turn_aborted", serde_json::json!({ "reason": "interrupted" }), now - 5.0 * min),
            event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), now - 4.0 * min),
        ]);
        add_thread(&db, "interrupted", &interrupted, now - 25.0 * min, "user");
        // ended 10 min ago without an answer of its own; a call came back after
        let quiet = dir.join("quiet.jsonl");
        write_lines(&quiet, &[
            event_at("task_complete", serde_json::json!({ "last_agent_message": "Before" }), now - 30.0 * min),
            event_at("task_started", serde_json::json!({}), now - 12.0 * min),
            event_at("task_complete", serde_json::json!({ "last_agent_message": null }), now - 10.0 * min),
            event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), now - 10.0 * min + 18_700.0),
        ]);
        add_thread(&db, "quiet", &quiet, now - 12.0 * min, "user");
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert!(inputs.problems.is_empty() && inputs.running.is_empty(), "{:?}", inputs.problems);
        assert_eq!(since(&inputs, "answered"), Some(now - min));
        assert_eq!(since(&inputs, "interrupted"), Some(now - 5.0 * min));
        assert_eq!(since(&inputs, "quiet"), Some(now - 10.0 * min));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // an answer that ends in a question asks, in bold or italics too
    #[test]
    fn an_answer_in_emphasis_that_ends_in_a_question_asks() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms().floor();
        let (dir, db) = codex_home("question");
        let long = format!("{} should I go on?", "word ".repeat(60));
        let cases = [
            ("plain", "Done.\n\nShould I proceed?".to_string()),
            ("bold", "Done.\n\n**Should I proceed?**".to_string()),
            ("italic", "Done.\n\n_Should I proceed?_".to_string()),
            ("long", format!("Done.\n\n{long}")),
            ("trailing", "**Should I proceed?** ".to_string()),
        ];
        for (id, message) in &cases {
            let path = dir.join(format!("{id}.jsonl"));
            write_lines(&path, &[event_at("task_complete", serde_json::json!({ "last_agent_message": message }), now - 60_000.0)]);
            add_thread(&db, id, &path, now - 120_000.0, "user");
        }
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert_eq!(inputs.sessions.len(), cases.len());
        for s in &inputs.sessions {
            assert!(!s.summary.needs_action.is_empty(), "{}", s.id);
        }
        assert!(!ends_in_question("Done.") && !ends_in_question("**Done.**") && !ends_in_question("?x"));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // what Codex writes after the answer, an item that came back late or the
    // thread's settings when it opens it again an hour on, leaves it as it was
    #[test]
    fn a_line_after_the_answer_leaves_its_since() {
        use std::io::Write;
        let _one = one_at_a_time();
        let (now, min) = (crate::util::now_ms().floor(), 60_000.0);
        let (dir, db) = codex_home("late");
        let path = dir.join("settled.jsonl");
        write_lines(&path, &[
            event_at("task_started", serde_json::json!({}), now - 70.0 * min),
            event_at("task_complete", serde_json::json!({ "last_agent_message": "Should I merge it?" }), now - 61.0 * min),
        ]);
        add_thread(&db, "settled", &path, now - 70.0 * min, "user");
        read(&dir, &mut Inputs::empty(now));
        // read on from what the cache has
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open");
        writeln!(file, "{}", event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), now - 60.5 * min)).expect("append");
        writeln!(file, "{}", event_at("thread_settings_applied", serde_json::json!({ "thread_id": "settled", "thread_settings": {} }), now - min)).expect("append");
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert_eq!(since(&inputs, "settled"), Some(now - 61.0 * min));
        drop(file);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_turn_counts_from_its_last_event() {
        let _one = one_at_a_time();
        let (now, min) = (crate::util::now_ms().floor(), 60_000.0);
        let (dir, db) = codex_home("running");
        let path = dir.join("busy.jsonl");
        write_lines(&path, &[
            event_at("task_complete", serde_json::json!({ "last_agent_message": "Before" }), now - 40.0 * min),
            event_at("task_started", serde_json::json!({}), now - 30.0 * min),
            event_at("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" } }), now - min),
        ]);
        add_thread(&db, "busy", &path, now - 30.0 * min, "user");
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert!(inputs.running.contains("codex_busy"));
        assert_eq!(since(&inputs, "busy"), Some(now - min));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollout_written_anew_is_read_again() {
        let _one = one_at_a_time();
        let dir = std::env::temp_dir().join(format!("pulse-rollout-anew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // an answer, then a turn with `tools` tool calls; `pad` makes each line longer
        let rollout = |answer: &str, tools: usize, pad: usize| {
            let pad = "x".repeat(pad);
            let mut lines = vec![
                event("task_complete", serde_json::json!({ "last_agent_message": answer, "pad": pad })),
                event("task_started", serde_json::json!({ "pad": pad })),
            ];
            lines.extend((0..tools).map(|_| event("item_completed", serde_json::json!({ "item": { "type": "CommandExecution" }, "pad": pad }))));
            lines.iter().map(|l| format!("{l}\n")).collect::<String>()
        };

        // the same size: a new file put in its place, as when Codex migrates a thread
        let path = dir.join("same.jsonl");
        std::fs::write(&path, rollout("First answer", 1, 0)).expect("write");
        assert_eq!(turn_of(&path).expect("turn").last_message.as_deref(), Some("First answer"));
        // not made in the same tick of the clock
        std::thread::sleep(Duration::from_millis(50));
        let new = dir.join("same.new");
        std::fs::write(&new, rollout("Other answer", 1, 0)).expect("write");
        assert_eq!(std::fs::metadata(&new).expect("meta").len(), std::fs::metadata(&path).expect("meta").len());
        std::fs::rename(&new, &path).expect("rename");
        let turn = turn_of(&path).expect("turn");
        assert!(turn.running);
        assert_eq!((turn.tools, turn.last_message.as_deref()), (1, Some("Other answer")));

        // larger, rewritten in place: where it was read to is now in the middle of a line
        let path = dir.join("larger.jsonl");
        std::fs::write(&path, rollout("Before", 10, 0)).expect("write");
        assert_eq!(turn_of(&path).expect("turn").tools, 10);
        let read = std::fs::metadata(&path).expect("meta").len() as usize;
        // what was appended is read on, not again
        assert!(line_starts(&path, read as u64) && !line_starts(&path, read as u64 - 1));
        let text = rollout("Rewritten", 11, 20);
        assert_ne!(text.as_bytes()[read - 1], b'\n');
        std::fs::write(&path, &text).expect("rewrite");
        let turn = turn_of(&path).expect("turn");
        assert!(turn.running);
        assert_eq!((turn.tools, turn.last_message.as_deref()), (11, Some("Rewritten")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_thread_no_longer_read_leaves_the_cache() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms();
        let (dir, db) = codex_home("evict");
        let rollout = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("{}\n", event("task_complete", serde_json::json!({ "last_agent_message": "Done" })))).expect("write");
            path
        };
        let (recent, old, archived, gone) = (rollout("recent.jsonl"), rollout("old.jsonl"), rollout("archived.jsonl"), rollout("gone.jsonl"));
        add_thread(&db, "recent", &recent, now - 60_000.0, "user");
        add_thread(&db, "old", &old, now - WINDOW_MS - 60_000.0, "user");
        add_thread(&db, "archived", &archived, now - 60_000.0, "user");
        db.execute("UPDATE threads SET archived = 1 WHERE id = 'archived'", []).expect("archive");
        // read while they were recent; gone's thread is no longer listed at all
        for path in [&old, &archived, &gone] {
            turn_of(path).expect("turn");
        }
        read(&dir, &mut Inputs::empty(now));
        let cached = |path: &PathBuf| CACHE.lock().expect("codex cache").contains_key(path);
        assert!(cached(&recent));
        assert!(!cached(&old) && !cached(&archived) && !cached(&gone));
        assert_eq!(*RECENT.lock().expect("codex recent"), BTreeSet::from([recent.clone()]));
        // the next read (the history's, say) reads the same, and keeps it
        read(&dir, &mut Inputs::empty(crate::util::now_ms()));
        assert!(cached(&recent));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_reads_old_archived_and_compressed_answers_without_changing_live_state() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms().floor();
        let old_at = now - WINDOW_MS - 60_000.0;
        let (dir, db) = codex_home("history");
        let thread = |id: &str, at: f64, answer: &str| {
            let path = dir.join(format!("{id}.jsonl"));
            write_lines(&path, &[
                event_at("task_started", serde_json::json!({}), at),
                event_at("task_complete", serde_json::json!({ "last_agent_message": answer }), at + 20_000.0),
            ]);
            add_thread(&db, id, &path, at, "user");
            path
        };
        let recent = thread("recent", now - 60_000.0, "A recent answer.");
        let old = thread("old", old_at, "Details.\n\nSTATUS: worked | The report is ready.");
        let archived = thread("archived", now - 90_000.0, "An earlier answer.");
        write_lines(&archived, &[
            event_at("task_complete", serde_json::json!({ "last_agent_message": "An earlier answer." }), now - 80_000.0),
            event_at("task_started", serde_json::json!({}), now - 75_000.0),
            event_at("turn_aborted", serde_json::json!({}), now - 70_000.0),
        ]);
        db.execute("UPDATE threads SET archived = 1 WHERE id = 'archived'", []).expect("archive");
        let compressed = thread("compressed", old_at - 60_000.0, "Details.\n\nThe archived report is ready.");
        let text = std::fs::read(&compressed).expect("read");
        std::fs::write(packed(&compressed), ruzstd::encoding::compress_to_vec(text.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)).expect("compress");
        std::fs::remove_file(&compressed).expect("remove plain");
        RESTING.lock().expect("cache").clear();

        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        assert!(RESTING.lock().expect("cache").is_empty());
        let before = serde_json::to_value(crate::items::build(&inputs, &crate::store::Store::default(), 0)).expect("snapshot");
        let history = crate::history::history(&inputs, &[]);
        let entry = |id: &str| history.iter().find(|e| e.id == format!("{PREFIX}{id}")).expect("entry");
        assert_eq!((entry("old").said.as_str(), entry("old").at), ("The report is ready.", old_at + 20_000.0));
        assert_eq!((entry("archived").said.as_str(), entry("archived").at), ("Interrupted", now - 70_000.0));
        assert!(entry("archived").archived);
        assert_eq!((entry("compressed").said.as_str(), entry("compressed").at), ("The archived report is ready.", old_at - 40_000.0));
        assert_eq!(entry("recent").said, "A recent answer.");
        assert_eq!(history.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["codex_recent", "codex_archived", "codex_old", "codex_compressed"]);
        assert!(inputs.running.is_empty() && inputs.in_turn.is_empty() && inputs.live.is_empty());
        assert_eq!(serde_json::to_value(crate::items::build(&inputs, &crate::store::Store::default(), 0)).expect("snapshot"), before);
        assert_eq!(*RECENT.lock().expect("recent"), BTreeSet::from([recent]));
        assert!(!CACHE.lock().expect("cache").contains_key(&old));

        let reads = HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst);
        let mut refreshed = Inputs::empty(now);
        read(&dir, &mut refreshed);
        crate::history::history(&refreshed, &[]);
        assert_eq!(HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst), reads);
        // A resumed thread and a removed thread no longer belong to this cache.
        db.execute("UPDATE threads SET recency_at_ms = ?1 WHERE id = 'old'", [now as i64]).expect("resume");
        db.execute("DELETE FROM threads WHERE id = 'archived'", []).expect("delete");
        read(&dir, &mut Inputs::empty(now));
        let cached = RESTING.lock().expect("cache");
        assert!(!cached.contains_key(&old) && !cached.contains_key(&archived));
        assert!(cached.contains_key(&packed(&compressed)));
        drop(cached);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parking_an_old_thread_reads_only_its_answer() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms().floor();
        let old_at = now - WINDOW_MS - 60_000.0;
        let (dir, db) = codex_home("park-history");
        let mut paths = Vec::new();
        for id in ["chosen", "other"] {
            let path = dir.join(format!("{id}.jsonl"));
            write_lines(&path, &[event_at("task_complete", serde_json::json!({ "last_agent_message": format!("The {id} answer.") }), old_at + 20_000.0)]);
            add_thread(&db, id, &path, old_at, "user");
            paths.push(path);
        }
        RESTING.lock().expect("cache").clear();
        let mut inputs = Inputs::empty(now);
        read(&dir, &mut inputs);
        let reads = HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst);
        // This is the same selected-entry conversion used by history_entries
        // before mark stores a parked card's title, project and sentence.
        let chosen = crate::history::selected(&inputs, &[], &["codex_chosen"]);
        assert_eq!(chosen.len(), 1);
        assert_eq!((chosen[0].said.as_str(), chosen[0].at), ("The chosen answer.", old_at + 20_000.0));
        assert_eq!(HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst), reads + 1);
        let cached = RESTING.lock().expect("cache");
        assert!(cached.contains_key(&paths[0]) && !cached.contains_key(&paths[1]));
        drop(cached);
        let full = crate::history::history(&inputs, &[]);
        assert_eq!(full.iter().find(|e| e.id == "codex_chosen").expect("chosen").said, chosen[0].said);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resting_answers_follow_rewrites_and_plain_or_compressed_sources() {
        let _one = one_at_a_time();
        let (dir, db) = codex_home("history-sources");
        let path = dir.join("answer.jsonl");
        let write = |answer: &str| write_lines(&path, &[event("task_complete", serde_json::json!({ "last_agent_message": answer }))]);
        let text = || said_at_rest(&path).map(|s| s.text);
        RESTING.lock().expect("cache").clear();
        write("First answer.");
        let size = std::fs::metadata(&path).expect("meta").len();
        assert_eq!(text().as_deref(), Some("First answer."));
        write("Other answer.");
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), size);
        // A deterministic metadata change, including on coarse filesystems.
        File::options().write(true).open(&path).expect("open").set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_900_000_000))).expect("mtime");
        assert_eq!(text().as_deref(), Some("Other answer."));
        let plain = std::fs::read(&path).expect("read");
        let zst = packed(&path);
        let compressed = ruzstd::encoding::compress_to_vec(plain.as_slice(), ruzstd::encoding::CompressionLevel::Fastest);
        std::fs::write(&zst, &compressed).expect("compress");
        std::fs::remove_file(&path).expect("remove plain");
        assert_eq!(text().as_deref(), Some("Other answer."));
        let reads = HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(text().as_deref(), Some("Other answer."));
        assert_eq!(HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst), reads);
        write("A newer plain answer.");
        assert_eq!(text().as_deref(), Some("A newer plain answer."));
        std::fs::remove_file(&path).expect("remove plain");
        std::fs::write(&zst, b"broken zstd").expect("corrupt");
        assert!(text().is_none());
        let reads = HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(text().is_none());
        assert_eq!(HISTORY_READS.load(std::sync::atomic::Ordering::SeqCst), reads);
        std::fs::write(&zst, &compressed).expect("restore");
        assert_eq!(text().as_deref(), Some("Other answer."));
        std::fs::remove_file(&zst).expect("remove compressed");
        assert!(text().is_none());
        write("Recovered answer.");
        assert_eq!(text().as_deref(), Some("Recovered answer."));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a compressed rollout cut off in its stream keeps the turn read up to there,
    // and the cache keeps it: it is not unpacked again on every refresh
    #[test]
    fn a_rollout_cut_off_in_its_stream_keeps_the_turn_before() {
        let _one = one_at_a_time();
        let (dir, db) = codex_home("cut");
        let path = dir.join("cut.jsonl");
        // more than the reader's chunk before the cut, in many blocks
        let mut lines = vec![event("task_complete", serde_json::json!({ "last_agent_message": "The answer before the cut." }))];
        lines.extend((0..40_000).map(|n| serde_json::json!({ "type": "response_item", "payload": { "n": n, "text": "some output of a long command" } })));
        lines.push(event("task_complete", serde_json::json!({ "last_agent_message": "The answer after the cut." })));
        write_lines(&path, &lines);
        let plain = std::fs::read(&path).expect("read");
        assert!(plain.len() as u64 > 2 * CHUNK);
        let compressed = ruzstd::encoding::compress_to_vec(plain.as_slice(), ruzstd::encoding::CompressionLevel::Fastest);
        let zst = packed(&path);
        std::fs::write(&zst, &compressed[..compressed.len() * 3 / 4]).expect("cut");
        std::fs::remove_file(&path).expect("remove plain");
        CACHE.lock().expect("cache").clear();
        RESTING.lock().expect("cache").clear();
        let turn = unpacked(&zst).expect("the turn before the cut");
        assert_eq!(turn.last_message.as_deref(), Some("The answer before the cut."));
        assert!(CACHE.lock().expect("cache").contains_key(&zst));
        assert_eq!(said_at_rest(&path).map(|s| s.text).as_deref(), Some("The answer before the cut."));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_sees_each_change_a_refresh_shows() {
        use std::io::Write;
        let _one = one_at_a_time();
        let now = crate::util::now_ms();
        let (dir, db) = codex_home("token");
        let line = format!("{}\n", event("task_complete", serde_json::json!({ "last_agent_message": "Done" })));
        let (first, second) = (dir.join("first.jsonl"), dir.join("second.jsonl"));
        std::fs::write(&first, &line).expect("write");
        std::fs::write(&second, &line).expect("write");
        add_thread(&db, "first", &first, now - 60_000.0, "user");
        read(&dir, &mut Inputs::empty(now));
        let mut last = token(&dir).expect("token");
        assert_eq!(token(&dir), Some(last), "nothing changed");
        let mut changed = |what: &str| {
            let now = token(&dir).expect("token");
            assert_ne!(now, last, "{what}");
            last = now;
        };
        let sql = |sql: &str| db.execute(sql, []).expect(sql);
        std::fs::OpenOptions::new().append(true).open(&first).expect("open").write_all(line.as_bytes()).expect("append");
        changed("a rollout grew");
        sql("UPDATE threads SET recency_at_ms = recency_at_ms + 1000");
        changed("a turn started");
        sql("UPDATE threads SET updated_at_ms = updated_at_ms + 2000");
        changed("a turn ended");
        sql("INSERT INTO thread_read_receipts (thread_id, first_unread_turn, revision) VALUES ('first', 'turn-2', 'r1')");
        changed("a turn not read yet");
        sql("UPDATE thread_read_receipts SET first_unread_turn = NULL, revision = 'r2'");
        changed("the turn read");
        sql("UPDATE threads SET name = 'Renamed'");
        changed("renamed");
        sql("UPDATE threads SET title = 'Another task'");
        changed("retitled");
        sql("UPDATE threads SET archived = 1");
        changed("archived");
        add_thread(&db, "second", &second, now, "user");
        changed("a new thread");
        add_thread(&db, "helper", &second, now, "subagent");
        assert_eq!(token(&dir), Some(last), "a subagent's thread is not shown");
        // without Codex it stays the same
        RECENT.lock().expect("codex recent").clear();
        let missing = dir.join("no-codex");
        assert!(token(&missing).is_some() && token(&missing) == token(&missing));
        sql("DROP TABLE thread_read_receipts");
        assert_eq!(token(&dir), None, "a database it can't read");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Codex compresses a rollout idle for some days and keeps its plain name:
    // the item stays as it was, a question open, a result with what came of it
    #[test]
    fn a_compressed_rollout_says_what_it_said() {
        use std::io::Write;

        use crate::items::{Stage, State};
        let _one = one_at_a_time();
        let (now, day) = (crate::util::now_ms().floor(), 24.0 * 3_600_000.0);
        let (dir, db) = codex_home("zst");
        let answered = |id: &str, answer: &str| {
            let path = dir.join(format!("{id}.jsonl"));
            write_lines(&path, &[
                event_at("task_started", serde_json::json!({}), now - 8.0 * day - 60_000.0),
                event_at("task_complete", serde_json::json!({ "last_agent_message": answer }), now - 8.0 * day),
            ]);
            add_thread(&db, id, &path, now - 8.0 * day - 60_000.0, "user");
            path
        };
        let question = answered("question", "I found two databases.\n\nDo you mean the staging database or production?");
        let result = answered("result", "Wrote the report.\n\nThe summary is in outputs.");
        db.execute("INSERT INTO thread_read_receipts (thread_id, first_unread_turn, revision) VALUES ('result', 'turn-1', 'r1')", []).expect("unread");
        let shown = || {
            let mut inputs = Inputs::empty(now);
            read(&dir, &mut inputs);
            let snapshot = crate::items::build(&inputs, &crate::store::Store::default(), 0);
            let item = |id: &str| snapshot.items.iter().find(|i| i.id == format!("{PREFIX}{id}")).map(|i| (i.state, i.stage, i.reason.clone()));
            (item("question"), item("result"), snapshot.counts.waiting)
        };
        let before = shown();
        assert_eq!(before, (
            Some((State::Waiting, Stage::Open, "Do you mean the staging database or production?".to_string())),
            Some((State::Review, Stage::Open, "The summary is in outputs.".to_string())),
            1,
        ));
        for path in [&question, &result] {
            let text = std::fs::read(path).expect("read");
            std::fs::write(packed(path), ruzstd::encoding::compress_to_vec(text.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)).expect("compress");
            std::fs::remove_file(path).expect("remove");
        }
        assert_eq!(shown(), before);
        // unpacked once: the next read takes it from the cache, the same size
        // and file (this one could not be unpacked now)
        let zst = packed(&question);
        let zeros = vec![0; std::fs::metadata(&zst).expect("meta").len() as usize];
        std::fs::OpenOptions::new().write(true).open(&zst).expect("open").write_all(&zeros).expect("zero");
        assert_eq!(shown(), before);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a thread in a Codex project goes by the project's name, a chat the app
    // made a folder for by none, any other by its folder
    #[test]
    fn a_thread_goes_by_its_codex_project_and_a_chat_by_none() {
        let _one = one_at_a_time();
        let now = crate::util::now_ms();
        let (dir, db) = codex_home("projects");
        let rollout = dir.join("done.jsonl");
        std::fs::write(&rollout, format!("{}\n", event_at("task_complete", serde_json::json!({ "last_agent_message": "Done" }), now - 60_000.0))).expect("write");
        db.execute_batch(
            "INSERT INTO projects (id, name) VALUES ('p1', 'Garden Planner'), ('p2', 'Seedling Trials');
             INSERT INTO project_roots (project_id, position, path) VALUES ('p1', 0, '/Users/me/Desktop/garden/scripts'),
                 ('p2', 0, '/Users/me/Desktop/garden/scripts/Old Ideas/Seedling');",
        )
        .expect("projects");
        let folders = [
            ("chat", "/Users/me/Documents/Codex/2026-01-15/what-year-did-the-bridge-open", ""),
            ("home", "/Users/me", "me"),
            ("repo", "/Users/me/work/shop-api", "shop-api"),
            ("notes", "/Users/me/Documents/Codex/notes/ab", "ab"),
            ("garden", "/Users/me/Desktop/garden/scripts", "Garden Planner"),
            ("inner", "/Users/me/Desktop/garden/scripts/Old Ideas/Seedling", "Seedling Trials"),
            ("below", "/Users/me/Desktop/garden/scripts/layout", "Garden Planner"),
            ("beside", "/Users/me/Desktop/garden/scripts2", "scripts2"),
            ("dated", "/Users/me/Documents/Notes/2026-01-15/ab", "ab"),
            ("work", "/Users/me/Work/Codex/2026-01-15/ab", "ab"),
            ("kept", "/Users/me/Documents/Codex/2026-01-15/kept-chat", ""),
        ];
        for (id, cwd, _) in folders {
            add_thread(&db, id, &rollout, now - 60_000.0, "user");
            db.execute("UPDATE threads SET cwd = ?1 WHERE id = ?2", [cwd, id]).expect("cwd");
        }
        let shown = || {
            let mut inputs = Inputs::empty(now);
            read(&dir, &mut inputs);
            assert!(inputs.problems.is_empty(), "{:?}", inputs.problems);
            let snapshot = crate::items::build(&inputs, &crate::store::Store::default(), 0);
            folders.map(|(id, _, _)| snapshot.items.iter().find(|i| i.id == format!("{PREFIX}{id}")).map(|i| i.project.clone()))
        };
        assert_eq!(shown(), folders.map(|(_, _, project)| Some(project.to_string())));
        // a root with no path holds nothing
        db.execute_batch("INSERT INTO projects (id, name) VALUES ('p3', 'Blank'); INSERT INTO project_roots (project_id, position, path) VALUES ('p3', 0, '');")
            .expect("blank");
        assert_eq!(shown(), folders.map(|(_, _, project)| Some(project.to_string())));
        // a project on the home folder holds what lies in it but a chat's own
        // folder, which only a project on that folder takes in
        db.execute_batch(
            "INSERT INTO projects (id, name) VALUES ('p4', 'Home'), ('p5', 'Kept');
             INSERT INTO project_roots (project_id, position, path) VALUES ('p4', 0, '/Users/me'),
                 ('p5', 0, '/Users/me/Documents/Codex/2026-01-15/kept-chat');",
        )
        .expect("home");
        let home = ["", "Home", "Home", "Home", "Garden Planner", "Seedling Trials", "Garden Planner", "Home", "Home", "Home", "Kept"];
        assert_eq!(shown(), home.map(|project| Some(project.to_string())));
        // a database from before Codex had projects: each by its folder
        db.execute_batch("DROP TABLE project_roots; DROP TABLE projects;").expect("drop");
        assert_eq!(shown()[4..6], [Some("scripts".to_string()), Some("Seedling".to_string())]);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a chat's own folder is in the system's Documents folder, whatever it is
    // named (moved, or OneDrive's in the system's language), or in one named so
    #[test]
    fn a_chat_is_in_the_documents_folder_wherever_it_is() {
        let chat = |cwd: &str, documents: &str| project(cwd, &[], Some(Path::new(documents)));
        let moved = "/Users/me/OneDrive/Dokumente";
        assert_eq!(chat("/Users/me/OneDrive/Dokumente/Codex/2026-01-15/ab", moved), Some(String::new()));
        assert_eq!(chat("/users/ME/onedrive/dokumente/Codex/2026-01-15/ab", "/Users/me/OneDrive/Dokumente/"), Some(String::new()));
        // not one inside it, nor the one around it
        assert_eq!(chat("/Users/me/OneDrive/Dokumente/Notes/Codex/2026-01-15/ab", moved), None);
        assert_eq!(chat("/Users/me/OneDrive/Dokumente/Codex/2026-01-15/ab", "/Users/me/OneDrive"), None);
        // one named Documents still is, wherever the system's is or without it
        assert_eq!(chat("/Users/me/Documents/Codex/2026-01-15/ab", moved), Some(String::new()));
        assert_eq!(project("/Users/me/Documents/Codex/2026-01-15/ab", &[], None), Some(String::new()));
        assert_eq!(project("/Users/me/OneDrive/Dokumente/Codex/2026-01-15/ab", &[], None), None);
        #[cfg(windows)]
        assert_eq!(chat(r"d:\MyDocs\Codex\2026-01-15\ab", r"D:\mydocs\"), Some(String::new()));
    }

    // the system's own Documents folder, by its name only: nothing is written there
    #[cfg(windows)]
    #[test]
    fn a_chat_is_in_the_systems_own_documents_folder() {
        let documents = documents().expect("the Documents folder");
        assert!(documents.is_absolute() && documents.is_dir(), "{documents:?}");
        let chat = documents.join(r"Codex\2026-01-15\ab");
        assert_eq!(project(&chat.to_string_lossy(), &[], Some(documents)), Some(String::new()));
        // a Codex folder in the home folder holds no chats
        let home = crate::util::home().expect("the home folder");
        assert_eq!(project(&home.join(r"Codex\2026-01-15\ab").to_string_lossy(), &[], Some(documents)), None);
    }
}
