// What Pulse reads: the Claude app's sessions and scheduled tasks, the runs
// the routine-recorder keeps, which sessions run now, their subagents and
// what they do right now, and the sidebar's unread dots.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use crate::background::background_of;
use crate::live::{live_of, Live};
use crate::procs::{commands, Command};
use crate::said::{said_of, Said};
use crate::running::{turn_at, STALE_MS};
use crate::runs::{RunRecord, Summary};
use crate::subagents::{subagents_of, SubagentView};
use crate::unread::read_unread;
use crate::util::{mtime_ms, now_ms, parse_time, paths, Paths};

const DAY_MS: f64 = 24.0 * 3_600_000.0;
// An unanswered call may wait through the reminder ladder and several daily
// reminders. Without a matching result, a crashed session can wait this long too.
const PENDING_MS: f64 = 7.0 * DAY_MS;
// sessions older than this are left out, unless they still run
pub const WINDOW_MS: f64 = 14.0 * DAY_MS;

#[derive(Clone, Default)]
pub struct Session {
    pub id: String,
    pub cli_id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    // the folder the session was started in; a worktree's session names its repository here
    pub origin_cwd: Option<String>,
    pub archived: bool,
    pub created_at: Option<f64>,
    pub last_activity: Option<f64>,
    pub last_focused: Option<f64>,
    pub task_id: Option<String>,
    // a side session: the session it was started from
    pub parent_id: Option<String>,
    pub summary: Summary,
}

#[derive(Clone)]
pub struct TaskDef {
    pub id: String,
    pub name: String,
    pub cron: Option<String>,
    pub fire_at: Option<f64>,
    pub enabled: bool,
    pub created_at: Option<f64>,
    pub cwd: Option<String>,
}

pub struct Inputs {
    pub now: f64,
    pub tasks: Vec<TaskDef>,
    // by task: when the app left a slot out because an earlier run was still open
    pub skips: HashMap<String, Vec<f64>>,
    pub sessions: Vec<Session>,
    pub records: Vec<Arc<RunRecord>>,
    pub running: HashSet<String>,
    // the running sessions that are in the middle of a turn (the others wait
    // for work in the background)
    pub in_turn: HashSet<String>,
    pub unread: HashSet<String>,
    pub subagents: HashMap<String, Vec<SubagentView>>,
    pub live: HashMap<String, Live>,
    // by session: the STATUS of its last answer
    pub said: HashMap<String, Said>,
    // the commands the sessions run now, from the process table
    pub commands: Vec<Command>,
    // by session with a command (its id in the transcript's name): what its
    // background ids were started for, and when its transcript last changed
    pub names: HashMap<String, HashMap<String, String>>,
    pub written: HashMap<String, f64>,
    // by session: the call that waits for the person's approval, since when
    pub awaiting: HashMap<String, (String, f64)>,
    // by session: the question it puts to the person in the middle of its turn
    pub asking: HashMap<String, (String, f64)>,
    pub problems: Vec<String>,
}

impl Inputs {
    pub fn empty(now: f64) -> Inputs {
        Inputs {
            now,
            tasks: Vec::new(),
            skips: HashMap::new(),
            sessions: Vec::new(),
            records: Vec::new(),
            running: HashSet::new(),
            in_turn: HashSet::new(),
            unread: HashSet::new(),
            subagents: HashMap::new(),
            live: HashMap::new(),
            said: HashMap::new(),
            commands: Vec::new(),
            names: HashMap::new(),
            written: HashMap::new(),
            awaiting: HashMap::new(),
            asking: HashMap::new(),
            problems: Vec::new(),
        }
    }
}

static JSON_CACHE: LazyLock<Mutex<HashMap<PathBuf, (f64, Arc<Value>)>>> = LazyLock::new(Default::default);
static SESSION_CACHE: LazyLock<Mutex<HashMap<PathBuf, (f64, Option<Session>)>>> = LazyLock::new(Default::default);
static RECORD_CACHE: LazyLock<Mutex<HashMap<PathBuf, (f64, Arc<RunRecord>)>>> = LazyLock::new(Default::default);

fn parse(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

// a file read before is taken from the cache while it is unchanged
fn cached<T: Clone>(cache: &Mutex<HashMap<PathBuf, (f64, T)>>, path: &Path, mtime: f64, read: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    if let Some((at, value)) = cache.lock().expect("cache").get(path) {
        if *at == mtime {
            return Ok(value.clone());
        }
    }
    let value = read()?;
    cache.lock().expect("cache").insert(path.to_path_buf(), (mtime, value.clone()));
    Ok(value)
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    mtime: f64,
}

fn entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut list = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let file_type = e.file_type()?;
        let path = e.path();
        let mtime = if file_type.is_file() { mtime_ms(&std::fs::metadata(&path)?) } else { 0.0 };
        list.push(Entry { name: e.file_name().to_string_lossy().into_owned(), path, is_dir: file_type.is_dir(), mtime });
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list)
}

pub fn transcript_of(projects: &Path, cli_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let file = format!("{cli_id}.jsonl");
    if let Some(cwd) = cwd {
        let folder: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let guess = projects.join(folder).join(&file);
        if guess.exists() {
            return Some(guess);
        }
    }
    // a worktree or a moved session: look through every project
    std::fs::read_dir(projects).ok()?.filter_map(Result::ok).map(|d| d.path().join(&file)).find(|p| p.exists())
}

fn string(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn number(m: &Map<String, Value>, key: &str) -> Option<f64> {
    m.get(key).and_then(Value::as_f64)
}

fn pick(raw: &Value) -> Option<Session> {
    let m = raw.as_object()?;
    let summary = m.get("postTurnSummary").and_then(Value::as_object);
    let parent = m.get("spawnedFrom").and_then(Value::as_object).and_then(|s| string(s, "sessionId"));
    Some(Session {
        id: string(m, "sessionId")?,
        cli_id: string(m, "cliSessionId"),
        title: string(m, "title"),
        cwd: string(m, "cwd"),
        origin_cwd: string(m, "originCwd"),
        archived: m.get("isArchived") == Some(&Value::Bool(true)),
        created_at: number(m, "createdAt"),
        last_activity: number(m, "lastActivityAt"),
        last_focused: number(m, "lastFocusedAt"),
        task_id: string(m, "scheduledTaskId"),
        parent_id: parent,
        summary: Summary {
            category: summary.and_then(|s| string(s, "status_category")),
            detail: summary.and_then(|s| string(s, "status_detail")).unwrap_or_default(),
            needs_action: summary.and_then(|s| string(s, "needs_action")).unwrap_or_default(),
        },
    })
}

fn task(raw: &Value) -> Option<TaskDef> {
    let m = raw.as_object()?;
    let id = string(m, "id")?;
    let fire_at = match m.get("fireAt") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => parse_time(s),
        _ => None,
    };
    Some(TaskDef {
        name: string(m, "displayName").unwrap_or_else(|| id.clone()),
        cron: string(m, "cronExpression"),
        fire_at,
        enabled: m.get("enabled") != Some(&Value::Bool(false)),
        created_at: number(m, "createdAt"),
        cwd: string(m, "cwd"),
        id,
    })
}

fn read_app(p: &Paths, inputs: &mut Inputs) -> std::io::Result<()> {
    for account in entries(&p.app)?.into_iter().filter(|e| e.is_dir) {
        for org in entries(&account.path)?.into_iter().filter(|e| e.is_dir) {
            for file in entries(&org.path)? {
                if file.name == "scheduled-tasks.json" {
                    let Ok(raw) = cached(&JSON_CACHE, &file.path, file.mtime, || parse(&file.path).map(Arc::new)) else {
                        inputs.problems.push(format!("{} unreadable", file.name));
                        continue;
                    };
                    inputs.tasks.extend(raw.get("scheduledTasks").and_then(Value::as_array).into_iter().flatten().filter_map(task));
                    for (id, list) in raw.get("recordedSkips").and_then(Value::as_object).into_iter().flatten() {
                        let ats = list.as_array().into_iter().flatten().filter_map(|s| s.get("at").and_then(Value::as_f64));
                        inputs.skips.entry(id.clone()).or_default().extend(ats);
                    }
                } else if file.name.starts_with("local_") && file.name.ends_with(".json") {
                    match cached(&SESSION_CACHE, &file.path, file.mtime, || parse(&file.path).map(|raw| pick(&raw))) {
                        Ok(Some(session)) => inputs.sessions.push(session),
                        Ok(None) => {}
                        Err(_) => inputs.problems.push(format!("{} unreadable", file.name)),
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn load_inputs() -> Result<Inputs, String> {
    let p = paths()?;
    let now = now_ms();
    Ok(load_from(&p, now, commands(now)))
}

fn load_from(p: &Paths, now: f64, commands: Vec<Command>) -> Inputs {
    let mut inputs = Inputs { commands, ..Inputs::empty(now) };
    if read_app(p, &mut inputs).is_err() {
        inputs.problems.push("The app's chat data was not found.".into());
    }
    match entries(&p.runs) {
        Ok(files) => {
            for file in files.into_iter().filter(|f| f.name.ends_with(".json")) {
                let read = || parse(&file.path).and_then(|v| serde_json::from_value::<RunRecord>(v).map_err(|e| e.to_string())).map(Arc::new);
                match cached(&RECORD_CACHE, &file.path, file.mtime, read) {
                    // records of runs long gone are not needed
                    Ok(record) if record.updated_at > now - WINDOW_MS => inputs.records.push(record),
                    Ok(_) => {}
                    Err(_) => inputs.problems.push(format!("Record {} unreadable", file.name)),
                }
            }
        }
        Err(_) => inputs.problems.push("No records of the routine-recorder found.".into()),
    }

    // a session runs while its transcript's last real entry is not a turn's
    // end, while one of its subagents still works in the background, or while
    // a command, workflow or monitor it started in the background is open
    for s in &inputs.sessions {
        let active = s.last_activity.unwrap_or(0.0) > now - PENDING_MS;
        let (Some(cli_id), false) = (&s.cli_id, s.archived) else { continue };
        // a session quiet for longer is not looked up, unless a command of its own still runs
        if !active && !inputs.commands.iter().any(|c| &c.session == cli_id) {
            continue;
        }
        let Some(path) = transcript_of(&p.projects, cli_id, s.cwd.as_deref()) else { continue };
        let open_at = turn_at(&path, false);
        let pending = open_at.is_some_and(|at| now - at < PENDING_MS);
        let agents = subagents_of(&path, now);
        let recent = s.last_activity.unwrap_or(0.0) > now - 6.0 * 3_600_000.0;
        // what runs in the background is open while its process runs, however old
        let alive: HashSet<&str> = inputs.commands.iter().filter(|c| &c.session == cli_id).map(|c| c.id.as_str()).collect();
        let background = background_of(&path, now, &alive);
        // a command of its own in the foreground: one the transcript does not name as background work
        let has_process = alive.iter().any(|id| !background.names.contains_key(*id));
        // A foreground process also keeps a quiet turn running, including a
        // command approved late. Explicit transcript endings still take priority.
        let in_turn = open_at.is_some_and(|at| (recent && now - at < STALE_MS) || has_process);
        if !alive.is_empty() {
            inputs.names.insert(cli_id.clone(), background.names);
            inputs.written.insert(cli_id.clone(), std::fs::metadata(&path).map(|m| mtime_ms(&m)).unwrap_or(0.0));
        }
        let waiting_on = background.open;
        let running = in_turn || agents.iter().any(|a| a.is_running) || !waiting_on.is_empty();
        if running {
            inputs.running.insert(s.id.clone());
            if in_turn {
                inputs.in_turn.insert(s.id.clone());
            }
        }
        if running || pending {
            if let Some(mut live) = live_of(&path) {
                if pending {
                    if let Some(question) = live.asks().filter(|(_, since)| now - since < PENDING_MS) {
                        inputs.asking.insert(s.id.clone(), question);
                    } else if let Some(call) = live.awaiting(now, has_process).filter(|(_, since)| now - since < PENDING_MS) {
                        inputs.awaiting.insert(s.id.clone(), call);
                    }
                }
                // the turn ended: what it waits for is the work in the background
                // (a command, workflow or monitor, else its subagents)
                if running && !in_turn {
                    live.tool = Some("in the background".into());
                    if let Some(task) = waiting_on.first() {
                        let more = if waiting_on.len() > 1 { format!(" (+{})", waiting_on.len() - 1) } else { String::new() };
                        live.tool = Some(format!("in the background · {}{more}", if task.what.is_empty() { "command" } else { &task.what }));
                        live.turn_started = Some(task.started);
                    }
                }
                if running {
                    inputs.live.insert(s.id.clone(), live);
                }
            }
        }
        if !agents.is_empty() {
            inputs.subagents.insert(s.id.clone(), agents);
        }
        if let Some(said) = said_of(&path) {
            inputs.said.insert(s.id.clone(), said);
        }
    }

    // the commands of sessions not looked at above (archived, long quiet): what
    // their transcript says of them
    let missing: HashSet<String> = inputs.commands.iter().map(|c| c.session.clone()).filter(|cli| !inputs.names.contains_key(cli)).collect();
    for cli_id in missing {
        let cwd = inputs.sessions.iter().find(|s| s.cli_id.as_deref() == Some(cli_id.as_str())).and_then(|s| s.cwd.clone());
        let Some(path) = transcript_of(&p.projects, &cli_id, cwd.as_deref()) else { continue };
        let alive: HashSet<&str> = inputs.commands.iter().filter(|c| c.session == cli_id).map(|c| c.id.as_str()).collect();
        let background = background_of(&path, now, &alive);
        inputs.written.insert(cli_id.clone(), std::fs::metadata(&path).map(|m| mtime_ms(&m)).unwrap_or(0.0));
        inputs.names.insert(cli_id, background.names);
    }

    // the sidebar's own unread dots; if they cannot be read, a guess: activity
    // after the last time the chat was opened
    inputs.unread = match read_unread(&p.leveldb) {
        Some(ids) => ids.into_iter().collect(),
        None => {
            inputs.problems.push("Read state estimated: the app's storage could not be read.".into());
            inputs
                .sessions
                .iter()
                .filter(|s| s.last_activity.unwrap_or(0.0) > s.last_focused.unwrap_or(0.0) + 60_000.0)
                .map(|s| s.id.clone())
                .collect()
        }
    };
    inputs
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::items::{build, Stage, State};
    use crate::store::Store;

    pub const PENDING_AT: f64 = 1_792_000_000_000.0;
    const SESSION: &str = "local_pending";
    static NEXT: AtomicU64 = AtomicU64::new(0);

    // Only synthetic files and injected commands: no installed app or process
    // scan is touched by these pipeline and notification tests.
    pub struct PendingFixture {
        root: PathBuf,
        paths: Paths,
        transcript: PathBuf,
    }

    impl PendingFixture {
        pub fn new(tool: &str) -> Self {
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("pulse-pending-{}-{nonce}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let paths = Paths { app: root.join("app"), runs: root.join("runs"), projects: root.join("projects"), leveldb: root.join("missing-leveldb") };
            let sessions = paths.app.join("account/org");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::create_dir_all(&paths.runs).unwrap();
            std::fs::create_dir_all(paths.projects.join("project")).unwrap();
            let raw = serde_json::json!({"sessionId": SESSION, "cliSessionId": "pending-cli", "title": "Pending approval", "cwd": "project",
                "createdAt": PENDING_AT, "lastActivityAt": PENDING_AT, "lastFocusedAt": PENDING_AT,
                "postTurnSummary": {"status_detail": "Previous work finished."}});
            std::fs::write(sessions.join("local_pending.json"), raw.to_string()).unwrap();
            let transcript = paths.projects.join("project/pending-cli.jsonl");
            let fixture = Self { root, paths, transcript };
            fixture.append(serde_json::json!({"type": "user", "message": {"role": "user", "content": "Run the tests."}}), PENDING_AT - 1_000.0);
            fixture.append(serde_json::json!({"type": "assistant", "message": {"stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "call", "name": tool, "input": {"command": "make test", "questions": [{"question": "Proceed?"}]}}
            ]}}), PENDING_AT);
            fixture
        }

        fn append(&self, mut line: Value, at: f64) {
            line["timestamp"] = chrono::DateTime::from_timestamp_millis(at as i64).unwrap().to_rfc3339().into();
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.transcript).unwrap();
            writeln!(file, "{line}").unwrap();
        }

        pub fn load(&self, now: f64) -> Inputs {
            self.with_commands(now, vec![])
        }

        fn with_commands(&self, now: f64, commands: Vec<Command>) -> Inputs {
            load_from(&self.paths, now, commands)
        }

        fn command(&self) -> Command {
            Command { session: "pending-cli".into(), id: "tests".into(), started: PENDING_AT, last_output: PENDING_AT, cpu: 0.0, hot_since: None, memory: 0, ports: vec![] }
        }
    }

    impl Drop for PendingFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn pending_approvals_outlast_the_running_cutoffs() {
        for tool in ["Bash", "PowerShell", "AskUserQuestion", "ExitPlanMode"] {
            let fixture = PendingFixture::new(tool);
            for age in [31.0 * 60_000.0, 121.0 * 60_000.0, 7.0 * 3_600_000.0, DAY_MS, PENDING_MS - 1.0] {
                let input = fixture.load(PENDING_AT + age);
                assert!(!input.running.contains(SESSION));
                assert!(!input.live.contains_key(SESSION));
                let snap = build(&input, &Store::default(), 0);
                let item = snap.items.iter().find(|i| i.id == SESSION).expect("pending survives the quiet turn");
                let question = matches!(tool, "AskUserQuestion" | "ExitPlanMode");
                assert_eq!((item.state, item.stage, item.since), (if question { State::Waiting } else { State::Blocked }, Stage::Open, PENDING_AT));
                if !question {
                    assert_eq!(item.reason, format!("Waiting for approval: {tool} · make test"));
                }
            }
            let expired = fixture.load(PENDING_AT + PENDING_MS);
            assert!(expired.asking.is_empty() && expired.awaiting.is_empty());
            // still in the window, but no longer waiting on anyone
            assert_eq!(build(&expired, &Store::default(), 0).items[0].stage, Stage::Done);
        }
    }

    #[test]
    fn pending_input_clears_when_the_transcript_goes_on_or_ends() {
        for event in [
            serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "call", "content": "ok"}]}}),
            serde_json::json!({"type": "user", "message": {"content": "A new request."}}),
            serde_json::json!({"type": "user", "message": {"content": "[Request interrupted by user]"}}),
            serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": []}}),
            serde_json::json!({"type": "system", "subtype": "stop_hook_summary"}),
            serde_json::json!({"type": "system", "subtype": "api_error"}),
            serde_json::json!({"type": "system", "subtype": "turn_duration"}),
        ] {
            let fixture = PendingFixture::new("Bash");
            let now = PENDING_AT + 7.0 * 3_600_000.0;
            assert!(fixture.load(now).awaiting.contains_key(SESSION));
            fixture.append(event, now);
            let input = fixture.load(now + 60_000.0);
            assert!(input.awaiting.is_empty() && input.asking.is_empty());
        }
    }

    #[test]
    fn unrelated_activity_does_not_extend_a_pending_calls_seven_days() {
        for tool in ["Bash", "AskUserQuestion", "ExitPlanMode"] {
            let fixture = PendingFixture::new(tool);
            let now = PENDING_AT + PENDING_MS;
            fixture.append(serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "Still waiting."}
            ]}}), now - 1_000.0);
            assert!(turn_at(&fixture.transcript, false).is_some());
            let input = fixture.load(now);
            assert!(input.asking.is_empty() && input.awaiting.is_empty());
            assert_eq!(build(&input, &Store::default(), 0).items[0].stage, Stage::Done);
        }
    }

    #[test]
    fn a_late_approved_foreground_command_runs_until_its_turn_ends() {
        let fixture = PendingFixture::new("Bash");
        for age in [31.0 * 60_000.0, 7.0 * 3_600_000.0] {
            let now = PENDING_AT + age;
            assert!(fixture.load(now).awaiting.contains_key(SESSION));
            let input = fixture.with_commands(now, vec![fixture.command()]);
            assert!(input.awaiting.is_empty());
            let snap = build(&input, &Store::default(), 0);
            assert_eq!((snap.items[0].state, snap.items[0].stage), (State::Running, Stage::Running));
        }
        let now = PENDING_AT + DAY_MS;
        fixture.append(serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": []}}), now);
        let input = fixture.with_commands(now, vec![fixture.command()]);
        assert!(!input.running.contains(SESSION));
        assert!(input.awaiting.is_empty());
    }
}
