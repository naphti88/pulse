// What a running session is doing right now, from the end of its transcript:
// when the current turn began, the tool call still waiting for its result,
// how many tools it called in this turn and how many of them failed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::running::CONVERSATION;
use crate::pending::Pending;
use crate::rules::limits::PATIENCE_MS;
use crate::rules::{INSTANT, QUESTIONS, SHELL};
use crate::util::{content, flag, json_lines, kind, parse_time, read_tail, text_blocks};

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    pub turn_started: Option<f64>,
    pub tool: Option<String>,
    pub tools: usize,
    pub errors: usize,
    // the calls without a result yet, the latest last
    #[serde(skip)]
    open: Vec<OpenCall>,
    // when the turn's latest call was written
    #[serde(skip)]
    call_at: f64,
    // when the turn's latest result came back, leaving out those of calls
    // behind the first one open
    #[serde(skip)]
    result_at: f64,
    // the id Claude Code gave the turn
    #[serde(skip)]
    prompt: Option<String>,
}

#[derive(Clone, Default)]
struct OpenCall {
    id: String,
    name: String,
    what: String,
    at: Option<f64>,
}

impl Live {
    // a Codex turn, as its rollout tells it (codex.rs)
    pub fn of_codex(turn_started: Option<f64>, tool: Option<String>, tools: usize) -> Live {
        Live { turn_started, tool, tools, ..Default::default() }
    }

    // a question the session puts to the person and waits for: (what, since when)
    pub fn asks(&self) -> Option<(String, f64)> {
        let call = self.open.last().filter(|c| QUESTIONS.contains(&c.name.as_str()))?;
        Some((call.what.clone(), call.at?))
    }

    // a request the plugin saw, while the call it is about is still open; one
    // from before this turn, or for a call that is over, waits for nobody
    pub fn pending(&self, pending: &Pending) -> Option<(String, f64)> {
        if self.turn_started.is_some_and(|started| pending.at < started) {
            return None;
        }
        if pending.agent.is_some() {
            return Some((format!("{} · in a subagent", pending.tool), pending.at));
        }
        let call = if pending.asks {
            self.open.iter().rev().find(|c| c.name.starts_with(&pending.tool))
        } else {
            self.open.iter().rev().find(|c| c.name == pending.tool)
        }?;
        let what = if pending.asks { format!("a question of {}", call.what) } else { call.what.clone() };
        Some((what, pending.at))
    }


    // the call that waits for the person's approval: a shell command open
    // for a few seconds without a process of the session running (has_process
    // says whether one runs in the foreground), or a quick tool open as long.
    // (what it is, since when): one answer's calls are asked about in the
    // order they were made, so the first one open is the one asked about,
    // from the answer's last call or, if later, from when the call before it
    // came back
    pub fn awaiting(&self, now: f64, has_process: bool) -> Option<(String, f64)> {
        let call = self.open.first()?;
        let since = call.at.map(|at| at.max(self.call_at).max(self.result_at)).filter(|since| now - since >= PATIENCE_MS)?;
        let is_waiting = INSTANT.contains(&call.name.as_str()) || (SHELL.contains(&call.name.as_str()) && !has_process);
        is_waiting.then(|| (call.what.clone(), since))
    }
}

// what a local command (/model, /clear …) writes: a caveat, its name, its output
fn is_command(text: &str) -> bool {
    text.starts_with("<local-command") || text.starts_with("<command-name>")
}

// a prompt the person typed, or a message another session sent (a
// subagent's hand-back): no tool result, no command or shell output, no
// summary a compaction wrote
pub fn is_prompt(line: &Value) -> bool {
    let is_peer = line.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("peer");
    if kind(line) != Some("user") || flag(line, "isSidechain") || flag(line, "isCompactSummary") || (flag(line, "isMeta") && !is_peer) {
        return false;
    }
    let has_result = content(line)
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")));
    let text = text_blocks(line);
    let text = text.trim_start();
    !has_result && !text.is_empty() && !text.starts_with("<bash-") && !is_command(text)
}

fn short(text: &str, n: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n - 1).collect::<String>())
    } else {
        line.to_string()
    }
}

// a tool call as one short line: its name and what it works on
fn describe(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block.get("input");
    let arg = ["file_path", "command", "pattern", "description", "url", "query", "prompt"]
        .iter()
        .find_map(|k| input.and_then(|i| i.get(*k)).and_then(Value::as_str))
        .map(|a| match Path::new(a).file_name() {
            Some(f) if a.starts_with('/') && !a.contains(' ') => f.to_string_lossy().into_owned(),
            _ => short(a, 48),
        });
    match arg {
        Some(a) if !a.is_empty() => format!("{name} · {a}"),
        _ => name.to_string(),
    }
}

// the first question of an AskUserQuestion call, and how many more follow
fn question_of(block: &Value) -> String {
    let questions = block.get("input").and_then(|i| i.get("questions")).and_then(Value::as_array);
    let first = questions.and_then(|q| q.first()).and_then(|q| q.get("question")).and_then(Value::as_str).unwrap_or("A question");
    let more = questions.map_or(0, |q| q.len().saturating_sub(1));
    if more > 0 { format!("{} (+{more})", short(first, 120)) } else { short(first, 120) }
}

// one entry of the transcript: a new prompt starts a new turn, a tool call
// or a failed result counts in the current one; a call is what it does only
// until its result comes back
fn apply(live: &mut Live, line: &Value) {
    // Claude Code gives each entry of a turn the id of the prompt that began
    // it, a compaction's summary too: another id begins a turn (a prompt, a
    // task's notification, another session's message, a picture sent without
    // words). A local command (/model) takes the id of the prompt typed after
    // it and begins nothing. Without ids, a prompt does; a first look that
    // starts in the middle of a turn knows of no start
    let id = line.get("promptId").and_then(Value::as_str).filter(|_| kind(line) == Some("user") && !flag(line, "isSidechain") && !is_command(text_blocks(line).trim_start()));
    let is_new = match (id, live.prompt.as_deref()) {
        (Some(id), Some(known)) => id != known,
        _ => is_prompt(line),
    };
    if is_new {
        let turn_started = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
        *live = Live { turn_started, prompt: id.map(str::to_string), ..Default::default() };
        return;
    }
    if live.prompt.is_none() {
        live.prompt = id.map(str::to_string);
    }
    // only the conversation's own entries: a request Claude Code sent
    // repeats calls and results it already holds, without their time
    if flag(line, "isSidechain") || !kind(line).is_some_and(|t| CONVERSATION.contains(&t)) {
        return;
    }
    let Some(blocks) = content(line).and_then(Value::as_array) else { return };
    let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                live.tools += 1;
                let id = b.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                let what = match name {
                    "AskUserQuestion" => question_of(b),
                    "ExitPlanMode" => "Plan to approve".to_string(),
                    _ => describe(b),
                };
                live.open.push(OpenCall { id: id.to_string(), name: name.to_string(), what, at });
                live.call_at = live.call_at.max(at.unwrap_or(0.0));
            }
            Some("tool_result") => {
                if b.get("is_error").and_then(Value::as_bool) == Some(true) {
                    live.errors += 1;
                }
                let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                // a call done beside the first one open leaves that one's
                // prompt as it is
                if !live.open.iter().skip(1).any(|open| open.id == id) {
                    live.result_at = live.result_at.max(at.unwrap_or(0.0));
                }
                live.open.retain(|open| open.id != id);
            }
            _ => {}
        }
    }
    live.tool = live.open.last().map(|open| open.what.clone());
}

// by transcript: how far it was read (the end of its last whole line) and what that gave
static CACHE: LazyLock<Mutex<HashMap<PathBuf, (u64, Live)>>> = LazyLock::new(Default::default);

// a transcript grows line by line while its session runs: after the first
// look only what was added since is read
pub fn live_of(transcript: &Path) -> Option<Live> {
    let size = std::fs::metadata(transcript).ok()?.len();
    let known = CACHE.lock().expect("live cache").get(transcript).cloned();
    let (from, mut live) = match known.clone() {
        Some((offset, live)) if offset == size => return Some(live),
        Some((offset, live)) if offset < size => (offset, live),
        // first look, or the file was rewritten: its last MB
        _ => (size.saturating_sub(1024 * 1024), Live::default()),
    };
    let buffer = read_tail(transcript, size, size - from).ok()?;
    // only whole lines: one still being written is read next time
    let end = buffer.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let text = String::from_utf8_lossy(&buffer[..end]);
    // a cut first line of a first look is partial
    let is_first_look = !matches!(known, Some((offset, _)) if offset <= size);
    let skip_first = is_first_look && from > 0;
    for line in json_lines(&text, skip_first) {
        apply(&mut live, &line);
    }
    CACHE.lock().expect("live cache").insert(transcript.to_path_buf(), (from + end as u64, live.clone()));
    Some(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    // a transcript's own fields are spelt here as the private check asks
    // (tools/check_private.py): written as JSON they read as a real transcript
    fn line(json: &str) -> Value {
        serde_json::from_str(&json.replace("\"prompt_id\":", "\"promptId\":")).expect("json")
    }

    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-10-30T10:00:00.000Z","message":{"content":"Mach das"}}"#;
    const USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make test"}}]}}"#;
    const RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;

    #[test]
    fn a_call_is_shown_only_until_its_result() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(USE));
        assert_eq!(live.tool.as_deref(), Some("Bash · make test"));
        apply(&mut live, &line(RESULT));
        assert_eq!(live.tool, None);
        assert_eq!(live.tools, 1);
    }

    #[test]
    fn parallel_calls_show_the_one_still_open() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(USE));
        apply(&mut live, &line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/a/b/notes.md"}}]}}"#));
        assert_eq!(live.tool.as_deref(), Some("Read · notes.md"));
        apply(&mut live, &line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"x"}]}}"#));
        assert_eq!(live.tool.as_deref(), Some("Bash · make test"));
        assert_eq!(live.errors, 1);
    }

    #[test]
    fn a_call_without_its_process_waits_for_approval() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make test"}}]}}"#));
        let at = parse_time("2026-10-30T10:00:01.000Z").expect("time");
        // a moment after the call, while a classifier may look at it, or with
        // its process running: it runs
        assert_eq!(live.awaiting(at + 1_000.0, false), None);
        assert_eq!(live.awaiting(at + 15_000.0, false), None);
        assert_eq!(live.awaiting(at + 60_000.0, true), None);
        assert_eq!(live.awaiting(at + 31_000.0, false), Some(("Bash · make test".to_string(), at)));
        // an edit open for seconds waits too; an MCP call is not judged
        apply(&mut live, &line(RESULT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"mcp__x__fetch","input":{"url":"https://example.com"}}]}}"#));
        assert_eq!(live.awaiting(at + 60_000.0, false), None);
    }

    #[test]
    fn the_second_call_of_one_answer_asks_once_the_first_came_back() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make test"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:20.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make deploy"}}]}}"#));
        let at = parse_time("2026-10-30T10:00:20.000Z").expect("time");
        // the person is asked about the first, from the answer's last call
        assert_eq!(live.awaiting(at + 15_000.0, false), None);
        assert_eq!(live.awaiting(at + 31_000.0, false), Some(("Bash · make test".to_string(), at)));
        // it ran: the second is asked about from then on
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:02:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        let ran = parse_time("2026-10-30T10:02:00.000Z").expect("time");
        assert_eq!(live.awaiting(ran + 15_000.0, false), None);
        assert_eq!(live.awaiting(ran + 31_000.0, false), Some(("Bash · make deploy".to_string(), ran)));
        // the next answer's call: from its own time
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:03:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:03:05.000Z","message":{"content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"make clean"}}]}}"#));
        let next = parse_time("2026-10-30T10:03:05.000Z").expect("time");
        assert_eq!(live.awaiting(next + 31_000.0, false), Some(("Bash · make clean".to_string(), next)));
    }

    #[test]
    fn a_call_back_beside_the_one_asked_about_moves_nothing() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/etc/hosts"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:02.000Z","message":{"content":[{"type":"tool_use","id":"w1","name":"WebFetch","input":{"url":"https://example.com","prompt":"x"}}]}}"#));
        let at = parse_time("2026-10-30T10:00:02.000Z").expect("time");
        let asked = Some(("Read · hosts".to_string(), at));
        assert_eq!(live.awaiting(at + 40_000.0, false), asked);
        // the fetch beside it comes back: the same prompt, from the same time
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:01:30.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"w1","content":"page"}]}}"#));
        let back = parse_time("2026-10-30T10:01:30.000Z").expect("time");
        assert_eq!(live.awaiting(back + 5_000.0, false), asked);
        assert_eq!(live.awaiting(back + 31_000.0, false), asked);
    }

    #[test]
    fn a_call_behind_one_not_judged_waits_its_turn() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__x__fetch","input":{"url":"https://example.com"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:02.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make test"}}]}}"#));
        let at = parse_time("2026-10-30T10:00:02.000Z").expect("time");
        // the MCP call runs; the command after it waits for it, not for the person
        assert_eq!(live.awaiting(at + 60_000.0, false), None);
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:01:30.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        let back = parse_time("2026-10-30T10:01:30.000Z").expect("time");
        assert_eq!(live.awaiting(back + 31_000.0, false), Some(("Bash · make test".to_string(), back)));
    }

    #[test]
    fn an_edit_behind_a_running_command_waits_for_it() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        // one answer: a command put in the background, back at once, a command
        // that runs for minutes, and an edit after it
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t1","name":"PowerShell","input":{"command":"npm run dev","run_in_background":true}}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:04.800Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make test"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:10.100Z","message":{"content":[{"type":"tool_use","id":"t3","name":"Edit","input":{"file_path":"/a/b/README.md"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:00:10.200Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        let at = parse_time("2026-10-30T10:00:01.000Z").expect("time");
        let back = parse_time("2026-10-30T10:00:10.200Z").expect("time");
        // the command runs: the edit waits for it, not for the person
        assert_eq!(live.awaiting(at + 40_000.0, true), None);
        assert_eq!(live.awaiting(at + 120_000.0, true), None);
        assert_eq!(live.awaiting(at + 309_000.0, true), None);
        // without its process, the command is the one asked about
        assert_eq!(live.awaiting(at + 40_000.0, false), Some(("Bash · make test".to_string(), back)));
    }

    #[test]
    fn a_call_back_from_before_the_first_look_moves_the_since() {
        // a first look that starts mid-answer: the call before this one was
        // written before the look, so only its result is read
        let mut live = Live::default();
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make deploy"}}]}}"#));
        let at = parse_time("2026-10-30T10:00:01.000Z").expect("time");
        // the look knows of no call before it
        assert_eq!(live.awaiting(at + 40_000.0, false), Some(("Bash · make deploy".to_string(), at)));
        // it came back: this one is asked about from then on
        apply(&mut live, &line(r#"{"type":"user","timestamp":"2026-10-30T10:00:50.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        let back = parse_time("2026-10-30T10:00:50.000Z").expect("time");
        assert_eq!(live.awaiting(back + 5_000.0, false), None);
        assert_eq!(live.awaiting(back + 31_000.0, false), Some(("Bash · make deploy".to_string(), back)));
    }

    #[test]
    fn a_request_copy_opens_no_call_and_fails_nothing() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(USE));
        apply(&mut live, &line(RESULT));
        // the next request repeats the call after its result: none is open
        apply(&mut live, &line(r#"{"type":"api-request-blob","sessionId":"s1","hash":"5e0f","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make test"}}]}}"#));
        assert_eq!((live.tool.as_deref(), live.tools), (None, 1));
        // a failed result, repeated just before it is written
        apply(&mut live, &line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/a/b/notes.md"}}]}}"#));
        apply(&mut live, &line(r#"{"type":"api-request-blob","sessionId":"s1","hash":"9c1d","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"x"}]}}"#));
        apply(&mut live, &line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"x"}]}}"#));
        assert_eq!((live.tool.as_deref(), live.tools, live.errors), (None, 2, 1));
    }

    #[test]
    fn a_request_the_plugin_saw_counts_while_its_call_is_open() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"m1","name":"mcp__mail__send","input":{}}]}}"#));
        let at = parse_time("2026-10-30T10:00:02.000Z").expect("time");
        let request = |tool: &str| Pending { tool: tool.into(), at, agent: None, asks: false, checked: false };
        assert_eq!(live.pending(&request("mcp__mail__send")), Some(("mcp__mail__send".to_string(), at)));
        // another tool, or one from before this turn: no
        assert_eq!(live.pending(&request("Bash")), None);
        assert_eq!(live.pending(&Pending { at: 5.0, ..request("mcp__mail__send") }), None);
        // a question of the server comes in the call it belongs to
        let question = Pending { asks: true, ..request("mcp__") };
        assert_eq!(live.pending(&question), Some(("a question of mcp__mail__send".to_string(), at)));
        // a subagent's call is not in this transcript
        assert!(live.pending(&Pending { agent: Some("a1".into()), ..request("Bash") }).is_some());
        // the call is over: whatever the file says
        apply(&mut live, &line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"m1","content":"ok"}]}}"#));
        assert_eq!(live.pending(&request("mcp__mail__send")), None);
    }

    #[test]
    fn a_choice_put_to_the_person_is_a_question() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"question":"Variante A oder B?"},{"question":"Gleich mergen?"}]}}]}}"#));
        let at = parse_time("2026-10-30T10:00:01.000Z").expect("time");
        assert_eq!(live.asks(), Some(("Variante A oder B? (+1)".to_string(), at)));
        apply(&mut live, &line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#));
        assert_eq!(live.asks(), None);
    }

    #[test]
    fn a_compaction_carries_on_the_turn_it_was_made_in() {
        let mut live = Live::default();
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","origin":{"kind":"task-notification"},"timestamp":"2026-10-30T12:00:00.000Z","message":{"role":"user","content":"<task-notification>\n<task-id>b7x</task-id>\n<status>completed</status>\n</task-notification>"}}"#));
        apply(&mut live, &line(r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{"trigger":"auto","precomputed":true},"timestamp":"2026-10-30T12:00:01.000Z"}"#));
        // a summary made ahead of time: written with the turn's id and its own, older time
        let summary = r#"{"type":"user","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"prompt_id":"p1","timestamp":"2026-10-30T11:30:00.000Z","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context."}}"#;
        apply(&mut live, &line(summary));
        apply(&mut live, &line(USE));
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-10-30T12:00:00.000Z"), 1));
        // without ids: a summary in the middle of a turn is no prompt either
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(USE));
        apply(&mut live, &line(&summary.replace(r#""prompt_id":"p1","#, "").replace("2026-10-30T11:30:00.000Z", "2026-10-30T10:20:00.000Z")));
        assert_eq!((live.turn_started, live.tools, live.tool.as_deref()), (parse_time("2026-10-30T10:00:00.000Z"), 1, Some("Bash · make test")));
    }

    #[test]
    fn a_message_from_another_session_begins_a_turn() {
        let mut live = Live::default();
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","timestamp":"2026-10-30T13:00:00.000Z","message":{"role":"user","content":"Mach das"}}"#));
        apply(&mut live, &line(USE));
        // a side chain's entry, with an id of its own, is no turn of the chat
        apply(&mut live, &line(r#"{"type":"user","isSidechain":true,"prompt_id":"p9","timestamp":"2026-10-30T13:01:00.000Z","message":{"role":"user","content":"Lies die Notizen"}}"#));
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-10-30T13:00:00.000Z"), 1));
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T13:05:00.000Z","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Fertig."}]}}"#));
        // a subagent in the background hands its report back
        let handback = r#"{"type":"user","isMeta":true,"prompt_id":"p2","origin":{"kind":"peer","from":"a13f","handback":true},"timestamp":"2026-10-30T14:07:00.000Z","message":{"role":"user","content":"Another Claude session sent a message:\n<agent-message from=\"a13f\">Bericht: fertig.</agent-message>"}}"#;
        apply(&mut live, &line(handback));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T14:08:00.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/a/b/notes.md"}}]}}"#));
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-10-30T14:07:00.000Z"), 1));
        // without ids too; what else the app adds as meta is no prompt
        assert!(is_prompt(&line(&handback.replace(r#""prompt_id":"p2","#, ""))));
        assert!(!is_prompt(&line(r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"[Image: source: /a/b/shot.png]"}]}}"#)));
    }

    #[test]
    fn a_picture_sent_without_words_begins_a_turn() {
        let mut live = Live::default();
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","timestamp":"2026-11-02T19:00:00.000Z","message":{"role":"user","content":"Mach das"}}"#));
        apply(&mut live, &line(USE));
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p2","origin":{"kind":"human"},"timestamp":"2026-11-02T19:30:00.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#));
        apply(&mut live, &line(r#"{"type":"user","isMeta":true,"prompt_id":"p2","timestamp":"2026-11-02T19:30:00.000Z","message":{"role":"user","content":[{"type":"text","text":"[Image: source: /a/b/shot.png]"}]}}"#));
        assert_eq!((live.turn_started, live.tools, live.tool.as_deref()), (parse_time("2026-11-02T19:30:00.000Z"), 0, None));
    }

    #[test]
    fn a_command_run_before_a_prompt_begins_no_turn() {
        // /model, written with the id of the prompt typed after it and its own, earlier time
        let command = [
            r#"{"type":"user","isMeta":true,"prompt_id":"p2","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"<local-command-caveat>The command below was run by the user.</local-command-caveat>"}}"#,
            r#"{"type":"user","prompt_id":"p2","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}}"#,
            r#"{"type":"user","prompt_id":"p2","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"<local-command-stdout>Set model to opus</local-command-stdout>"}}"#,
        ];
        let prompt = r#"{"type":"user","prompt_id":"p2","origin":{"kind":"human"},"timestamp":"2026-11-04T11:00:00.000Z","message":{"role":"user","content":"Mach das"}}"#;
        let mut live = Live::default();
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","timestamp":"2026-11-04T09:00:00.000Z","message":{"role":"user","content":"Erst das"}}"#));
        apply(&mut live, &line(USE));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-11-04T09:05:00.000Z","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Fertig."}]}}"#));
        for entry in command {
            apply(&mut live, &line(entry));
        }
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-11-04T09:00:00.000Z"), 1));
        apply(&mut live, &line(prompt));
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-11-04T11:00:00.000Z"), 0));
        // nor does a first look take the turn's id from it
        let mut live = Live::default();
        for entry in command.into_iter().chain([prompt]) {
            apply(&mut live, &line(entry));
        }
        assert_eq!(live.turn_started, parse_time("2026-11-04T11:00:00.000Z"));
    }

    #[test]
    fn a_first_look_in_the_middle_of_a_turn_knows_of_no_start() {
        let mut live = Live::default();
        // the look starts after the prompt: a result tells the turn, not when it began
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","timestamp":"2026-11-04T10:05:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t0","content":"ok"}]}}"#));
        apply(&mut live, &line(USE));
        assert_eq!((live.turn_started, live.tools), (None, 1));
        // the next turn is known by its id
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p1","timestamp":"2026-11-04T10:06:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#));
        apply(&mut live, &line(r#"{"type":"user","prompt_id":"p2","origin":{"kind":"human"},"timestamp":"2026-11-04T10:07:00.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#));
        assert_eq!((live.turn_started, live.tools), (parse_time("2026-11-04T10:07:00.000Z"), 0));
    }
}
