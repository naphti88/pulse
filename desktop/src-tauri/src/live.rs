// What a running session is doing right now, from the end of its transcript:
// when the current turn began, the tool call still waiting for its result,
// how many tools it called in this turn and how many of them failed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use serde_json::Value;

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
}

#[derive(Clone, Default)]
struct OpenCall {
    id: String,
    name: String,
    what: String,
    at: Option<f64>,
}

impl Live {
    // the call that waits for the person's approval: a shell command open
    // for a few seconds without a process of the session running (has_process
    // says whether one runs in the foreground), or a quick tool open as long
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

    // (what it is, since when)
    pub fn awaiting(&self, now: f64, has_process: bool) -> Option<(String, f64)> {
        let call = self.open.last()?;
        let at = call.at.filter(|at| now - at >= PATIENCE_MS)?;
        let is_waiting = INSTANT.contains(&call.name.as_str()) || (SHELL.contains(&call.name.as_str()) && !has_process);
        is_waiting.then(|| (call.what.clone(), at))
    }
}

// a prompt the person typed: no tool result, no command or shell output
pub fn is_prompt(line: &Value) -> bool {
    if kind(line) != Some("user") || flag(line, "isSidechain") || flag(line, "isMeta") {
        return false;
    }
    let has_result = content(line)
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")));
    let text = text_blocks(line);
    let text = text.trim_start();
    !has_result && !text.is_empty() && !text.starts_with("<bash-") && !text.starts_with("<local-command") && !text.starts_with("<command-name>")
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
    if is_prompt(line) {
        *live = Live { turn_started: line.get("timestamp").and_then(Value::as_str).and_then(parse_time), ..Default::default() };
        return;
    }
    if flag(line, "isSidechain") {
        return;
    }
    let Some(blocks) = content(line).and_then(Value::as_array) else { return };
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                live.tools += 1;
                let id = b.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
                let what = match name {
                    "AskUserQuestion" => question_of(b),
                    "ExitPlanMode" => "Plan to approve".to_string(),
                    _ => describe(b),
                };
                live.open.push(OpenCall { id: id.to_string(), name: name.to_string(), what, at });
            }
            Some("tool_result") => {
                if b.get("is_error").and_then(Value::as_bool) == Some(true) {
                    live.errors += 1;
                }
                let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
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

    fn line(json: &str) -> Value {
        serde_json::from_str(json).expect("json")
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
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"mcp__x__fetch","input":{"url":"https://example.com"}}]}}"#));
        assert_eq!(live.awaiting(at + 60_000.0, false), None);
    }

    #[test]
    fn a_request_the_plugin_saw_counts_while_its_call_is_open() {
        let mut live = Live::default();
        apply(&mut live, &line(PROMPT));
        apply(&mut live, &line(r#"{"type":"assistant","timestamp":"2026-10-30T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"m1","name":"mcp__mail__send","input":{}}]}}"#));
        let at = parse_time("2026-10-30T10:00:02.000Z").expect("time");
        let request = |tool: &str| Pending { tool: tool.into(), at, agent: None, asks: false };
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
}
