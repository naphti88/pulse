// A case of what happened in a session, with everything private taken out:
// it keeps the shape a rule looks at (which entries came, in which order, which
// tools, how a turn ended) and none of what was said. Nothing is copied unless
// it is known harmless: what is not listed in testdata/allowed.json does not
// leave here, and a case that does not pass that list is not written at all.
//
// tools/check_private.py holds every file in testdata/ to the same list, so a
// case that is committed has passed it twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use serde_json::{json, Map, Value};

use crate::engine::Engine;
use crate::failed::Failed;
use crate::items::Item;
use crate::live::is_prompt;
use crate::said::status_of;
use crate::util::{parse_time, read_tail};

const ALLOWED: &str = include_str!("../testdata/allowed.json");
// every case starts here: when a session ran is not part of it
const BASE: &str = "2026-01-01T00:00:00.000Z";
const NOTHING: &str = "…";

pub fn captures() -> Result<PathBuf, String> {
    Ok(crate::util::support_dir()?.join("captures"))
}

// a template of the list against a text: {n} stands for digits
fn fits(template: &str, text: &str) -> bool {
    let mut parts = template.split("{n}");
    let Some(first) = parts.next() else { return false };
    let Some(mut rest) = text.strip_prefix(first) else { return false };
    let holes = template.matches("{n}").count();
    for (i, part) in parts.enumerate() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        if i + 1 == holes && part.is_empty() {
            return rest.is_empty();
        }
        match rest.strip_prefix(part) {
            Some(after) => rest = after,
            None => return false,
        }
    }
    rest.is_empty()
}

pub struct Allowed {
    keys: Vec<String>,
    values: Vec<String>,
    templates: Vec<String>,
}

impl Allowed {
    pub fn load() -> Allowed {
        let list = |raw: &Value, key: &str| -> Vec<String> {
            raw[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
        };
        let raw: Value = serde_json::from_str(ALLOWED).expect("testdata/allowed.json");
        Allowed { keys: list(&raw, "keys"), values: list(&raw, "values"), templates: list(&raw, "templates") }
    }

    fn is_value(&self, text: &str) -> bool {
        self.values.iter().any(|v| v == text) || self.templates.iter().any(|t| fits(t, text))
    }

    // what is not on the list, by where it is; never what it says
    pub fn problems(&self, value: &Value, at: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    if self.keys.iter().any(|k| k == key) {
                        self.problems(inner, &format!("{at}.{key}"), out);
                    } else {
                        out.push(format!("unknown key at {at}"));
                    }
                }
            }
            Value::Array(list) => {
                for (i, inner) in list.iter().enumerate() {
                    self.problems(inner, &format!("{at}[{i}]"), out);
                }
            }
            Value::String(text) if !self.is_value(text) => out.push(format!("a value not on the list at {at}")),
            _ => {}
        }
    }
}

// real ids become t1, t2, …, the same id the same number; a workflow's keeps
// its prefix, which is how it is told apart
#[derive(Default)]
struct Ids(HashMap<String, usize>);

impl Ids {
    fn of(&mut self, id: &str) -> String {
        let next = self.0.len() + 1;
        let n = *self.0.entry(id.to_string()).or_insert(next);
        if id.starts_with("wf_") {
            format!("wf_t{n}")
        } else {
            format!("t{n}")
        }
    }
}

struct Scrubber {
    allowed: Allowed,
    ids: Ids,
    first: Option<f64>,
}

// the id after a marker, up to what cannot be in one
fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|id| !id.is_empty())
}

impl Scrubber {
    fn new() -> Scrubber {
        Scrubber { allowed: Allowed::load(), ids: Ids::default(), first: None }
    }

    // only signals a reader looks for survive, each as a fixed text with a neutral id
    fn text(&mut self, text: &str) -> String {
        let start = text.trim_start();
        if start.starts_with("[Request interrupted") {
            return "[Request interrupted by user]".into();
        }
        for (prefix, kept) in [("<bash-", "<bash-input>…"), ("<local-command", "<local-command-stdout>…"), ("<command-name", "<command-name>…")] {
            if start.starts_with(prefix) {
                return kept.into();
            }
        }
        if let Some(said) = status_of(text) {
            return format!("{NOTHING}\nSTATUS: {} | {NOTHING}", said.state.as_str());
        }
        if let Some(id) = after(text, "running in background with ID: ") {
            return format!("running in background with ID: {}", self.ids.of(id));
        }
        if let Some(id) = after(text, "Monitor started (task ") {
            return format!("Monitor started (task {}", self.ids.of(id));
        }
        if text.contains("Workflow") {
            if let Some(id) = after(text, "Task ID: ") {
                return format!("Workflow Task ID: {}", self.ids.of(id));
            }
        }
        if let Some(id) = after(text, "<task-id>") {
            let status = text.split("<status>").nth(1).and_then(|s| s.split('<').next()).unwrap_or("");
            if matches!(status, "completed" | "failed" | "killed" | "stopped") {
                return format!("<task-notification><task-id>{}</task-id><status>{status}</status></task-notification>", self.ids.of(id));
            }
        }
        NOTHING.into()
    }

    fn time(&mut self, line: &Value) -> Option<String> {
        let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time)?;
        let first = *self.first.get_or_insert(at);
        let base = parse_time(BASE)? + (at - first).max(0.0);
        Some(Utc.timestamp_millis_opt(base as i64).single()?.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
    }

    fn tool(&self, name: &str) -> String {
        if name.starts_with("mcp__") {
            "mcp__server__tool".into()
        } else if self.allowed.values.iter().any(|v| v == name) {
            name.into()
        } else {
            "OtherTool".into()
        }
    }

    fn content(&mut self, content: &Value) -> Value {
        match content {
            Value::String(text) => json!(self.text(text)),
            Value::Array(blocks) => Value::Array(blocks.iter().filter_map(|b| self.block(b)).collect()),
            _ => json!(NOTHING),
        }
    }

    fn block(&mut self, block: &Value) -> Option<Value> {
        let text = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default();
        match block.get("type").and_then(Value::as_str)? {
            "text" => Some(json!({"type": "text", "text": self.text(text("text"))})),
            "thinking" | "redacted_thinking" => Some(json!({"type": "thinking"})),
            "tool_use" => {
                let name = self.tool(text("name"));
                let mut input = Map::new();
                let given = block.get("input");
                if given.and_then(|i| i.get("run_in_background")).and_then(Value::as_bool) == Some(true) {
                    input.insert("run_in_background".into(), json!(true));
                }
                if let Some(task) = given.and_then(|i| i.get("task_id")).and_then(Value::as_str) {
                    input.insert("task_id".into(), json!(self.ids.of(task)));
                }
                Some(json!({"type": "tool_use", "id": self.ids.of(text("id")), "name": name, "input": input}))
            }
            "tool_result" => {
                let mut out = json!({"type": "tool_result", "tool_use_id": self.ids.of(text("tool_use_id"))});
                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    out["is_error"] = json!(true);
                }
                out["content"] = block.get("content").map_or(json!(NOTHING), |c| self.content(c));
                Some(out)
            }
            _ => None,
        }
    }

    fn line(&mut self, line: &Value) -> Value {
        let kind = line.get("type").and_then(Value::as_str).unwrap_or("other");
        let is_known = self.allowed.values.iter().any(|v| v == kind);
        let mut out = Map::new();
        out.insert("type".into(), json!(if is_known { kind } else { "other" }));
        if let Some(subtype) = line.get("subtype").and_then(Value::as_str).filter(|s| self.allowed.values.iter().any(|v| v == s)) {
            out.insert("subtype".into(), json!(subtype));
        }
        if let Some(at) = self.time(line) {
            out.insert("timestamp".into(), json!(at));
        }
        for flag in ["isSidechain", "isMeta"] {
            if line.get(flag).and_then(Value::as_bool) == Some(true) {
                out.insert(flag.into(), json!(true));
            }
        }
        if let Some(message) = line.get("message") {
            let mut kept = Map::new();
            if let Some(reason) = message.get("stop_reason").and_then(Value::as_str).filter(|s| self.allowed.values.iter().any(|v| v == s)) {
                kept.insert("stop_reason".into(), json!(reason));
            }
            if let Some(content) = message.get("content") {
                kept.insert("content".into(), self.content(content));
            }
            out.insert("message".into(), Value::Object(kept));
        }
        // a notification of work in the background comes as a queue entry
        if kind == "queue-operation" {
            if let Some(text) = line.get("content").and_then(Value::as_str) {
                out.insert("content".into(), json!(self.text(text)));
            }
        }
        Value::Object(out)
    }
}

pub struct Options {
    // the line to stop after (from 1), none: the end of the transcript
    pub until: Option<usize>,
    pub age_ms: f64,
    // false, true: a command of its own runs in the foreground, or "background":
    // the last command it left in the background still runs
    pub process: Value,
    pub pending: Option<Value>,
    pub engine: Option<Value>,
    pub failed: Option<Value>,
    pub expect: Value,
}

// the last turn of a transcript as a case; a case that does not pass the list is refused
pub fn case_from(transcript: &Path, options: Options) -> Result<Value, String> {
    let text = std::fs::read_to_string(transcript).map_err(|e| e.to_string())?;
    let mut lines: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    if let Some(until) = options.until {
        lines.truncate(until);
    }
    let start = lines.iter().rposition(is_prompt).unwrap_or(0);
    let mut scrubber = Scrubber::new();
    let scrubbed: Vec<Value> = lines[start..].iter().map(|l| scrubber.line(l)).collect();
    let mut case = json!({"ageMs": options.age_ms, "process": options.process, "expect": options.expect, "lines": scrubbed});
    if let Some(pending) = options.pending {
        case["pending"] = pending;
    }
    if let Some(engine) = options.engine {
        case["engine"] = engine;
    }
    if let Some(failed) = options.failed {
        case["failed"] = failed;
    }
    let mut problems = Vec::new();
    scrubber.allowed.problems(&case, "$", &mut problems);
    match problems.first() {
        Some(problem) => Err(format!("The case was not written: {problem}")),
        None => Ok(case),
    }
}

// the time of the last entry that has one
fn last_time(transcript: &Path) -> Option<f64> {
    let size = std::fs::metadata(transcript).ok()?.len();
    let tail = read_tail(transcript, size, size.min(64 * 1024)).ok()?;
    let text = String::from_utf8_lossy(&tail);
    text.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find_map(|l| l.get("timestamp").and_then(Value::as_str).and_then(parse_time))
}

// writes a case of what an item is now where it stays on this Mac, outside any
// repository; to become a test it has to be moved to testdata/ by hand
pub fn save_case(transcript: &Path, item: &Item, has_process: bool, engine: Option<&Engine>, failed: Option<&Failed>, now: f64) -> Result<PathBuf, String> {
    let engine = engine.map(|e| json!({"status": e.status, "waitingFor": e.waiting_for}));
    let failed = failed.map(|f| json!({"error": f.error}));
    let expect = json!({"rule": item.why.rule, "state": item.state, "stage": item.stage});
    let age_ms = last_time(transcript).map_or(0.0, |at| (now - at).max(0.0));
    let case = case_from(transcript, Options { until: None, age_ms, process: json!(has_process), pending: None, engine, failed, expect })?;
    let dir = captures()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}-{}.json", item.why.rule, now as i64));
    std::fs::write(&path, serde_json::to_vec_pretty(&case).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubbed(line: Value) -> String {
        Scrubber::new().line(&line).to_string()
    }

    #[test]
    fn nothing_that_was_said_or_named_survives() {
        let line = json!({
            "type": "assistant", "timestamp": "2026-10-30T10:00:00.000Z", "uuid": "SECRET",
            "sessionId": "SECRET", "cwd": "/SECRET/path", "gitBranch": "SECRET", "version": "SECRET",
            "message": {"id": "SECRET", "model": "SECRET", "stop_reason": "tool_use", "usage": {"a": "SECRET"}, "content": [
                {"type": "text", "text": "SECRET plain words"},
                {"type": "thinking", "thinking": "SECRET", "signature": "SECRET"},
                {"type": "tool_use", "id": "toolu_SECRET", "name": "Bash", "input": {"command": "SECRET", "description": "SECRET"}},
                {"type": "tool_use", "id": "toolu_2", "name": "mcp__SECRET__tool", "input": {"to": "SECRET"}},
                {"type": "tool_use", "id": "toolu_3", "name": "SECRETtool", "input": {"to": "SECRET"}},
                {"type": "tool_result", "tool_use_id": "toolu_SECRET", "content": [{"type": "text", "text": "SECRET"}, {"type": "image", "source": "SECRET"}]},
                {"type": "SECRET", "text": "SECRET"},
            ]},
        });
        // fields of a real transcript; check_private stops them written out in a file
        let mut line = line;
        for field in [concat!("parent", "Uuid"), concat!("toolUse", "Result"), concat!("request", "Id")] {
            line[field] = json!({"a": "SECRET"});
        }
        let out = scrubbed(line);
        assert!(!out.contains("SECRET"), "{out}");
        assert!(out.contains("\"stop_reason\":\"tool_use\""), "{out}");
    }

    #[test]
    fn a_line_of_a_kind_nobody_knows_keeps_nothing() {
        let out = scrubbed(json!({"type": "SECRET-kind", "subtype": "SECRET", "content": "SECRET", "attachment": {"a": "SECRET"}, "isSidechain": "SECRET"}));
        assert_eq!(out, r#"{"type":"other"}"#);
        // a status sentence is words of the person's work; a mention in a sentence is no status
        let said = Scrubber::new().text("Fertig.\n\nSTATUS: waiting | SECRET Kunde wartet");
        assert_eq!(said, "…\nSTATUS: waiting | …");
        assert_eq!(Scrubber::new().text("Der STATUS: worked steht im SECRET Satz"), "…");
        // the id is replaced, the rest of the sentence is gone
        let mut s = Scrubber::new();
        assert_eq!(s.text("Started SECRET, running in background with ID: b1unpd24f. SECRET"), "running in background with ID: t1");
        assert_eq!(s.text("<task-notification><task-id>b1unpd24f</task-id><status>SECRET</status></task-notification>"), "…");
    }

    #[test]
    fn the_time_of_day_of_a_session_is_not_kept() {
        let mut s = Scrubber::new();
        let at = |t: &str| json!({"type": "user", "timestamp": t});
        assert_eq!(s.line(&at("2026-10-30T17:45:10.000Z"))["timestamp"], "2026-01-01T00:00:00.000Z");
        assert_eq!(s.line(&at("2026-10-30T17:45:40.500Z"))["timestamp"], "2026-01-01T00:00:30.500Z");
    }

    #[test]
    fn the_list_refuses_what_it_does_not_know() {
        let allowed = Allowed::load();
        let problems = |v: Value| {
            let mut out = Vec::new();
            allowed.problems(&v, "$", &mut out);
            out
        };
        assert!(problems(json!({"type": "assistant", "id": "t12", "timestamp": "2026-01-01T00:00:30.500Z"})).is_empty());
        assert_eq!(problems(json!({"secret": "assistant"})).len(), 1);
        assert_eq!(problems(json!({"type": "Müller GmbH"})).len(), 1);
        assert_eq!(problems(json!({"id": "t12 and more"})).len(), 1);
        assert_eq!(problems(json!({"id": "t"})).len(), 1);
        assert_eq!(problems(json!({"id": "xt12"})).len(), 1);
    }

    #[test]
    fn every_rule_is_a_value_of_the_list() {
        let allowed = Allowed::load();
        for id in crate::rules::STATE_RULES.iter().map(|r| r.id).chain(crate::rules::STAGE_RULES.iter().map(|r| r.id)) {
            assert!(allowed.is_value(id), "{id}");
        }
    }
}

// the cases in testdata/, each played through the same readers and rules as a
// real session
#[cfg(test)]
mod cases {
    use super::*;
    use crate::inputs::{read_session, Inputs, Session};
    use crate::engine::Engine;
use crate::failed::Failed;
    use crate::items::build;
    use crate::procs::Command;
    use crate::store::Store;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");

    pub fn play(case: &Value, name: &str) -> Item {
        let lines = case["lines"].as_array().expect("lines");
        let dir = std::env::temp_dir().join(format!("pulse-case-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("session.jsonl");
        let mut text = lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        text.push('\n');
        std::fs::write(&path, text).expect("transcript");
        let last = lines.iter().rev().find_map(|l| l.get("timestamp").and_then(Value::as_str).and_then(parse_time)).expect("a time");
        let now = last + case["ageMs"].as_f64().unwrap_or(0.0);
        let session = Session { id: "local_case".into(), cli_id: Some(name.into()), last_activity: Some(now), created_at: Some(now), ..Session::default() };
        let mut inputs = Inputs::empty(now);
        let process = match &case["process"] {
            Value::Bool(true) => Some("foreground".to_string()),
            Value::String(_) => lines.iter().rev().find_map(|l| after(&l.to_string(), "running in background with ID: ").map(str::to_string)),
            _ => None,
        };
        if let Some(id) = process {
            inputs.commands.push(Command { session: name.into(), id, started: now, last_output: now, cpu: 0.0, hot_since: None, memory: 0, ports: Vec::new(), left: false });
        }
        // a case with an engine: the records of the processes are there, and say this
        if let Some(status) = case["engine"]["status"].as_str() {
            inputs.engines.known = true;
            if status != "none" {
                let waiting_for = case["engine"]["waitingFor"].as_str().map(str::to_string);
                inputs.engines.by_id.insert(name.to_string(), Engine { status: status.to_string(), waiting_for, since: now });
            }
        }
        if let Some(error) = case["failed"]["error"].as_str() {
            inputs.stopped.insert(name.to_string(), Failed { error: error.to_string(), at: now });
        }
        if let Some(tool) = case["pending"]["tool"].as_str() {
            let report = crate::pending::Pending { tool: tool.into(), at: now, agent: None, asks: false, checked: false };
            inputs.reported.insert(name.to_string(), vec![report]);
        }
        read_session(&mut inputs, &session, &path);
        inputs.sessions.push(session);
        std::fs::remove_dir_all(&dir).expect("clean up");
        build(&inputs, &Store::default(), 60).items.remove(0)
    }

    fn word<T: serde::Serialize>(value: T) -> String {
        serde_json::to_value(value).expect("value").as_str().expect("a word").to_string()
    }

    #[test]
    fn every_case_is_clean_and_comes_out_as_expected() {
        let allowed = Allowed::load();
        let mut seen = 0;
        for entry in std::fs::read_dir(DIR).expect("testdata") {
            let path = entry.expect("entry").path();
            let name = path.file_stem().expect("name").to_string_lossy().into_owned();
            if name == "allowed" {
                continue;
            }
            let case: Value = serde_json::from_slice(&std::fs::read(&path).expect("case")).expect("json");
            let mut problems = Vec::new();
            allowed.problems(&case, "$", &mut problems);
            assert!(problems.is_empty(), "{name}: {problems:?}");
            let item = play(&case, &name);
            let expect = &case["expect"];
            assert_eq!(item.why.rule, expect["rule"].as_str().unwrap_or(""), "{name}: {:?}", item.why);
            assert_eq!(word(item.state), expect["state"].as_str().unwrap_or(""), "{name}");
            assert_eq!(word(item.stage), expect["stage"].as_str().unwrap_or(""), "{name}");
            seen += 1;
        }
        assert!(seen > 0);
    }

    // makes cases from real transcripts, by a list only this Mac has
    // (PULSE_CASES: [{file, path, until, ageMs, process, rule, stage, state}]);
    // what it writes has passed the list
    #[test]
    #[ignore]
    fn make_cases() {
        let plan: Value = serde_json::from_slice(&std::fs::read(std::env::var("PULSE_CASES").expect("PULSE_CASES")).expect("plan")).expect("json");
        for entry in plan.as_array().expect("a list") {
            let text = |key: &str| entry[key].as_str().unwrap_or("").to_string();
            let expect = json!({"rule": text("rule"), "state": text("state"), "stage": text("stage")});
            let options = Options {
                until: entry["until"].as_u64().map(|n| n as usize),
                age_ms: entry["ageMs"].as_f64().unwrap_or(0.0),
                process: entry["process"].clone(),
                pending: entry.get("pending").cloned().filter(|p| !p.is_null()),
                engine: entry.get("engine").cloned().filter(|p| !p.is_null()),
                failed: entry.get("failed").cloned().filter(|p| !p.is_null()),
                expect,
            };
            let case = case_from(Path::new(&text("path")), options).expect("a case");
            let item = play(&case, &text("file"));
            assert_eq!(item.why.rule, text("rule"), "{}: {:?}", text("file"), item.why);
            let body = serde_json::to_vec_pretty(&case).expect("json");
            std::fs::write(Path::new(DIR).join(format!("{}.json", text("file"))), body).expect("write");
        }
    }
}
