// What Pulse reads: the Claude app's sessions and scheduled tasks, the runs
// the routine-recorder keeps, which sessions run now, their subagents and
// what they do right now, and the sidebar's unread dots.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use crate::background::background_of;
use crate::drift;
use crate::engine::{engines, Engine, Engines, INPUT_NEEDED};
use crate::failed::{failed_in, in_words, Failed};
use crate::live::{live_of, Live};
use crate::pending::{pending_in, Pending};
use crate::procs::{commands, Command};
use crate::rules::limits::{ACTIVE_MS, LIVE_MS, PATIENCE_MS, WINDOW_MS};
use crate::rules::{Word, SHELL};
use crate::said::{said_of, Said};
use crate::running::is_running;
use crate::runs::{RunRecord, Summary};
use crate::subagents::{subagents_of, SubagentView};
use crate::unread::read_unread;
use crate::util::{mtime_ms, now_ms, parse_time, paths, Paths};
use crate::work;

#[derive(Clone, Default)]
pub struct Session {
    pub id: String,
    pub cli_id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    // the folders the app noted its shell worked in
    pub folders: Vec<String>,
    // the branches the app noted it wrote to (repository, branch) and the
    // branches of its PRs
    pub written: Vec<(String, String)>,
    pub prs: Vec<String>,
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
    // by the session's id in its transcript's name: what the plugin saw stand
    // before the person, and by session what of it is still open
    pub engines: Engines,
    // by session: what the session itself says waits for the person, since when
    pub engine_approval: HashMap<String, (String, f64)>,
    pub engine_question: HashMap<String, (String, f64)>,
    // by session: what it changed that has not landed, in words
    pub work: HashMap<String, Vec<String>>,
    pub reported: HashMap<String, Vec<Pending>>,
    pub pending: HashMap<String, (String, f64)>,
    // by the session's id in its transcript's name: the API error that ended
    // its turn, and by session what of it still stands
    pub stopped: HashMap<String, Failed>,
    pub failed: HashMap<String, (String, f64)>,
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
            engines: Engines::default(),
            engine_approval: HashMap::new(),
            engine_question: HashMap::new(),
            work: HashMap::new(),
            reported: HashMap::new(),
            pending: HashMap::new(),
            stopped: HashMap::new(),
            failed: HashMap::new(),
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
    let strings = |key: &str| m.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    // "<repository>\0<branch>"
    let written = strings("writtenBranches").filter_map(|w| w.split_once('\0')).map(|(dir, branch)| (dir.to_string(), branch.to_string())).collect();
    let prs = m.get("prs").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("branch")?.as_str()).map(str::to_string).collect();
    let parent = m.get("spawnedFrom").and_then(Value::as_object).and_then(|s| string(s, "sessionId"));
    Some(Session {
        id: string(m, "sessionId")?,
        cli_id: string(m, "cliSessionId"),
        title: string(m, "title"),
        cwd: string(m, "cwd"),
        folders: strings("shellFolders").map(str::to_string).collect(),
        origin_cwd: string(m, "originCwd"),
        archived: m.get("isArchived") == Some(&Value::Bool(true)),
        created_at: number(m, "createdAt"),
        last_activity: number(m, "lastActivityAt"),
        last_focused: number(m, "lastFocusedAt"),
        task_id: string(m, "scheduledTaskId"),
        parent_id: parent,
        written,
        prs,
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
                    if raw.get("scheduledTasks").is_none_or(|tasks| !tasks.is_array()) {
                        inputs.problems.push("The Claude app changed its list of routines. Pulse needs an update.".into());
                    }
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

// a report that still says something: a shell that runs was let through, and
// what was only sent to be asked for takes as long as the classifier may, and
// is over when the process has spoken since
fn stands(report: &Pending, now: f64, has_process: bool, engine: Option<&Engine>) -> bool {
    !(has_process && SHELL.contains(&report.tool.as_str()))
        && (!report.checked || (now - report.at >= PATIENCE_MS && engine.is_none_or(|e| e.since <= report.at)))
}

// a session runs while its transcript's last real entry is not a turn's end,
// while one of its subagents still works in the background, or while a
// command, workflow or monitor it started in the background is open
pub fn read_session(inputs: &mut Inputs, s: &Session, path: &Path) {
    let now = inputs.now;
    let Some(cli_id) = &s.cli_id else { return };
    let agents = subagents_of(path, now);
    let recent = s.last_activity.unwrap_or(0.0) > now - LIVE_MS;
    // the session's own word; without a record of its process it cannot be in a
    // turn; without any such records the transcript is all there is
    let engine = inputs.engines.by_id.get(cli_id).or(inputs.engines.by_id.get(&s.id)).cloned();
    let in_turn = match &engine {
        Some(engine) => engine.is_in_turn(),
        None => !inputs.engines.known && recent && is_running(path, now, false),
    };
    // what runs in the background is open while its process runs, however old
    let alive: HashSet<&str> = inputs.commands.iter().filter(|c| &c.session == cli_id).map(|c| c.id.as_str()).collect();
    let background = background_of(path, now, &alive);
    // a command of its own in the foreground: one the transcript does not name as background work
    let has_process = alive.iter().any(|id| !background.names.contains_key(*id));
    let is_alive = !alive.is_empty();
    let waiting_on = background.open;
    if is_alive {
        inputs.names.insert(cli_id.clone(), background.names);
        inputs.written.insert(cli_id.clone(), std::fs::metadata(path).map(|m| mtime_ms(&m)).unwrap_or(0.0));
    }
    if let Some(failed) = inputs.stopped.get(cli_id).filter(|_| !in_turn) {
        inputs.failed.insert(s.id.clone(), (in_words(&failed.error).to_string(), failed.at));
    }
    if in_turn || agents.iter().any(|a| a.is_running) || !waiting_on.is_empty() {
        inputs.running.insert(s.id.clone());
        if in_turn {
            inputs.in_turn.insert(s.id.clone());
        }
        if let Some(mut live) = live_of(path) {
            if in_turn {
                if let Some(question) = live.asks() {
                    inputs.asking.insert(s.id.clone(), question);
                } else if let Some(engine) = engine.as_ref().filter(|e| e.is_waiting()) {
                    let what = (live.tool.clone().unwrap_or_else(|| "a tool".into()), engine.since);
                    let asks = engine.waiting_for.as_deref() == Some(INPUT_NEEDED);
                    if asks { &mut inputs.engine_question } else { &mut inputs.engine_approval }.insert(s.id.clone(), what);
                } else if let Some(call) = inputs.reported.get(cli_id).and_then(|list| {
                    // its busy does not say nothing waits: the app asks for some
                    // tools itself, and the process goes on saying busy
                    list.iter().rev().filter(|p| stands(p, now, has_process, engine.as_ref())).find_map(|p| live.pending(p))
                }) {
                    inputs.pending.insert(s.id.clone(), call);
                } else if let Some(call) = live.awaiting(now, has_process) {
                    inputs.awaiting.insert(s.id.clone(), call);
                }
            }
            // the turn ended: what it waits for is the work in the background
            // (a command, workflow or monitor, else its subagents)
            if !in_turn {
                live.tool = Some("in the background".into());
                if let Some(task) = waiting_on.first() {
                    let more = if waiting_on.len() > 1 { format!(" (+{})", waiting_on.len() - 1) } else { String::new() };
                    live.tool = Some(format!("in the background · {}{more}", if task.what.is_empty() { "command" } else { &task.what }));
                    live.turn_started = Some(task.started);
                }
            }
            inputs.live.insert(s.id.clone(), live);
        }
    }
    if !agents.is_empty() {
        inputs.subagents.insert(s.id.clone(), agents);
    }
    if let Some(said) = said_of(path) {
        inputs.said.insert(s.id.clone(), said);
    }
}

pub fn load_inputs() -> Result<Inputs, String> {
    let p = paths()?;
    let now = now_ms();
    let mut inputs = Inputs { commands: commands(now), reported: pending_in(&p.pending), stopped: failed_in(&p.failed), engines: engines(&p.engines), ..Inputs::empty(now) };
    if inputs.engines.changed {
        inputs.problems.push("Claude Code changed its process records. Pulse needs an update.".into());
    }
    if read_app(&p, &mut inputs).is_err() {
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

    let sessions = std::mem::take(&mut inputs.sessions);
    let mut transcripts: Vec<(f64, PathBuf)> = Vec::new();
    let mut by_session: HashMap<String, PathBuf> = HashMap::new();
    for s in &sessions {
        let active = s.last_activity.unwrap_or(0.0) > now - ACTIVE_MS;
        if !s.archived && active {
            let path = s.cli_id.as_deref().and_then(|cli_id| transcript_of(&p.projects, cli_id, s.cwd.as_deref()));
            if let Some(path) = path {
                read_session(&mut inputs, s, &path);
                by_session.insert(s.id.clone(), path.clone());
                transcripts.push((s.last_activity.unwrap_or(0.0), path));
            }
        }
    }
    inputs.sessions = sessions;
    inputs.work = work::left(&inputs.sessions, &by_session, &inputs.running, now);
    let changed = drift::sessions(&inputs.sessions, now, transcripts.len(), inputs.said.len());
    inputs.problems.extend(changed);
    if let Some((_, newest)) = transcripts.iter().max_by(|a, b| a.0.total_cmp(&b.0)) {
        inputs.problems.extend(drift::transcript_file(newest));
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
    keep_open(&mut inputs, &p.projects);
    Ok(inputs)
}

// sessions long quiet are not read any more, but what they left open must not
// get lost: a question or a block, or the answer of a chat nobody read
fn keep_open(inputs: &mut Inputs, projects: &Path) {
    let now = inputs.now;
    let quiet: Vec<(String, String, Option<String>, bool)> = inputs
        .sessions
        .iter()
        .filter(|s| !s.archived && s.last_activity.unwrap_or(0.0) <= now - ACTIVE_MS)
        .filter_map(|s| Some((s.id.clone(), s.cli_id.clone()?, s.cwd.clone(), s.task_id.is_none())))
        .collect();
    for (id, cli_id, cwd, is_chat) in quiet {
        let Some(path) = transcript_of(projects, &cli_id, cwd.as_deref()) else { continue };
        let Some(said) = said_of(&path) else { continue };
        if still_open(said.state, is_chat, inputs.unread.contains(&id)) {
            inputs.said.insert(id, said);
        }
    }
}

fn still_open(word: Word, is_chat: bool, unread: bool) -> bool {
    match word {
        Word::Waiting | Word::Blocked => true,
        Word::Worked => is_chat && unread,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_stays_of_a_session_long_quiet() {
        assert!(still_open(Word::Waiting, false, false));
        assert!(still_open(Word::Blocked, true, false));
        assert!(still_open(Word::Worked, true, true));
        // read, or a routine's result nobody will come back to
        assert!(!still_open(Word::Worked, true, false));
        assert!(!still_open(Word::Worked, false, true));
        assert!(!still_open(Word::Idle, true, true));
    }

    #[test]
    fn a_report_stands_until_something_says_it_is_over() {
        let report = |tool: &str, checked| Pending { tool: tool.into(), at: 1000.0, agent: None, asks: false, checked };
        let engine = |since| Engine { status: "busy".into(), waiting_for: None, since };
        let later = 1000.0 + PATIENCE_MS;
        // the app's own dialog: the process said nothing since
        assert!(stands(&report("mcp__Claude_Browser__preview_start", true), later, false, Some(&engine(500.0))));
        // a classifier may still let it through, or did, and the process went on
        assert!(!stands(&report("mcp__x__y", true), later - 1.0, false, Some(&engine(500.0))));
        assert!(!stands(&report("mcp__x__y", true), later, false, Some(&engine(2000.0))));
        // a shell that runs was let through, whatever was reported
        assert!(!stands(&report("Bash", false), later, true, None));
        assert!(stands(&report("Bash", false), later, false, None));
    }
}
