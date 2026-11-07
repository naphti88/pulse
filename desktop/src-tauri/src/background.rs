// The background work of a session that goes on after its turn ended: a
// command run in the background, a workflow, a monitor. Each gets an id when
// it starts ("running in background with ID: …", "Workflow launched in
// background. Task ID: …", "Monitor started (task …"); a <task-notification>
// with that id and a final status ends it. A session with such work open is
// still running: it wakes up when the work is done. A command or monitor runs
// as long as its process does (the process table says so, src/procs.rs); a
// workflow has none and counts by the transcript alone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::rules::limits::WORKFLOW_MAX_AGE_MS;
use crate::util::{parse_time, read_tail};

// what the result of a Workflow call says when the workflow went to the
// background, on this machine or in a remote session
const LAUNCHED: [&str; 2] = ["Workflow launched in background. Task ID: ", "Workflow launched in a remote CCR session. Task ID: "];
// a request as Claude Code sent it: it repeats messages the transcript holds,
// several in one line and without their time
const REQUEST: &str = "{\"type\":\"api-request-blob\"";

#[derive(Clone)]
pub struct Task {
    pub what: String,
    pub started: f64,
    // a monitor ends on its own after its time
    pub expires: Option<f64>,
    // a workflow runs inside Claude Code: no process of its own tells that it runs
    pub workflow: bool,
}

#[derive(Clone, Default)]
struct Scan {
    // how far the transcript was read: the end of its last whole line
    offset: u64,
    open: HashMap<String, Task>,
    // the description of a background call, by its tool use id, until its result
    calls: HashMap<String, String>,
    // the description of a shell call, by its tool use id, until its result:
    // it names only what its timeout, or the user, moves to the background
    shells: HashMap<String, String>,
    // the tool use ids of Workflow calls, until their result
    flows: HashSet<String>,
    ended: HashSet<String>,
    // what each background id was started for, open or ended
    names: HashMap<String, String>,
}

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Scan>>> = LazyLock::new(Default::default);

// the text after a marker up to the first character that cannot be in an id
fn id_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|id| !id.is_empty())
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{name}\":\"");
    let start = line.find(&key)? + key.len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(59).collect::<String>())
    } else {
        line.to_string()
    }
}

// the line after "Summary: " in a result, read from the JSON: the raw line
// escapes its quotes and ends the line in a "\n"
fn summary(line: &str) -> Option<String> {
    let entry = serde_json::from_str::<Value>(line).ok()?;
    let blocks = entry.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)?;
    let result = blocks.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))?;
    let text = match result.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => return None,
    };
    text.lines().find_map(|l| l.strip_prefix("Summary: ")).map(short).filter(|s| !s.is_empty())
}

// the text of a line that hands on notifications, read from the JSON: one
// queued (a queue-operation, an attachment) or a user's message of its own
fn notified(line: &str) -> Option<String> {
    let entry = serde_json::from_str::<Value>(line).ok()?;
    let text = match entry.get("type")?.as_str()? {
        "queue-operation" => entry.get("content"),
        "attachment" => entry.get("attachment").and_then(|a| a.get("prompt")),
        "user" => entry.get("message").and_then(|m| m.get("content")),
        _ => None,
    };
    text?.as_str().map(String::from)
}

impl Scan {
    fn line(&mut self, line: &str) {
        if line.starts_with(REQUEST) {
            return;
        }
        // a background call, or a shell call that its timeout may move to the
        // background: remember what it is for, by its tool use id
        if line.contains("\"tool_use\"") && ["\"run_in_background\":true", "\"name\":\"Monitor\"", "\"name\":\"Workflow\"", "\"name\":\"Bash\"", "\"name\":\"PowerShell\""].iter().any(|m| line.contains(m)) {
            if let Ok(entry) = serde_json::from_str::<Value>(line) {
                let blocks = entry.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
                for b in blocks.into_iter().flatten().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")) {
                    let input = b.get("input");
                    let text = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
                    let what = text("description").or(text("command")).or(text("scriptPath")).or(text("name")).unwrap_or("");
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                    // the model may send the flag as a string: Claude Code reads "true" as true
                    let flag = input.and_then(|i| i.get("run_in_background"));
                    let background = matches!(name, "Monitor" | "Workflow") || flag.and_then(Value::as_bool) == Some(true) || flag.and_then(Value::as_str) == Some("true");
                    if let Some(id) = b.get("id").and_then(Value::as_str) {
                        if background {
                            self.calls.insert(id.to_string(), short(what));
                        } else if matches!(name, "Bash" | "PowerShell") {
                            self.shells.insert(id.to_string(), short(what));
                        }
                        if name == "Workflow" {
                            self.flows.insert(id.to_string());
                        }
                    }
                }
            }
        }
        // its result names the id it runs under
        if line.contains("\"tool_result\"") {
            let call = field(line, "tool_use_id");
            // a workflow is launched by its own call: a result that only quotes
            // the launch (a grep, a file read) launches nothing
            let flow = call.is_some_and(|u| self.flows.remove(u));
            let what = call.and_then(|u| self.calls.remove(u)).unwrap_or_default();
            let shell = call.and_then(|u| self.shells.remove(u)).unwrap_or_default();
            // a command that outran its timeout, or that the user sent there by
            // hand (Ctrl+B), goes on in the background, under the id the
            // result's own record names: named for the card by its shell call,
            // not opened, as its process tells that it runs
            if let Some(id) = field(line, "backgroundTaskId").filter(|id| line.contains(&format!("was moved to the background (ID: {id})")) || line.contains(&format!("was manually backgrounded by user with ID: {id}."))) {
                self.names.insert(id.to_string(), shell);
            }
            // (a launch of an older wording names only its "Task ID: ")
            let launched = if flow { LAUNCHED.iter().find_map(|m| id_after(line, m)).or_else(|| id_after(line, "Task ID: ")) } else { None };
            let id = launched
                .or_else(|| id_after(line, "running in background with ID: "))
                .or_else(|| id_after(line, "Monitor started (task "));
            // the line that starts the work names it first: a later one only quotes it
            if let Some(id) = id.filter(|id| !self.ended.contains(*id) && !self.open.contains_key(*id)) {
                let started = field(line, "timestamp").and_then(parse_time).unwrap_or(0.0);
                let workflow = launched.is_some();
                // a monitor says when it ends; a workflow's summary may say "expires in" of anything
                let expires = if workflow { None } else { id_after(line, "expires in ").and_then(|m| m.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok()).map(|m| started + m * 60_000.0) };
                // a workflow's call holds its script, or the path of one: its summary names it
                let what = if workflow { summary(line).unwrap_or_else(|| "workflow".into()) } else { what };
                self.names.insert(id.to_string(), what.clone());
                self.open.insert(id.to_string(), Task { what, started, expires, workflow });
            }
        }
        // a notification ends it, as does stopping it. Claude Code queues one
        // and hands it on as a user's message: a result or a call that holds
        // one only quotes it. Its id and status are read inside it, so that one
        // without a status takes none from the next, and an id told of outside
        // one ends nothing
        let told = if line.contains("<task-notification>") { notified(line) } else { None };
        let mut rest = told.as_deref().unwrap_or("");
        while let Some(at) = rest.find("<task-notification>") {
            rest = &rest[at + 19..];
            let Some(end) = rest.find("</task-notification>") else { break };
            let own = &rest[..end];
            // one cut short (the next begins before it ends) has no end of its own
            if own.contains("<task-notification>") {
                continue;
            }
            if let Some(id) = id_after(own, "<task-id>") {
                let status = own.find("<status>").map(|s| &own[s + 8..]).and_then(|s| s.split('<').next()).unwrap_or("");
                if !matches!(status, "running" | "pending" | "") {
                    self.open.remove(id);
                    self.ended.insert(id.to_string());
                }
            }
            rest = &rest[end..];
        }
        if line.contains("\"name\":\"TaskStop\"") {
            if let Some(id) = id_after(line, "\"task_id\":\"") {
                self.open.remove(id);
                self.ended.insert(id.to_string());
            }
        }
    }
}

pub struct Background {
    // the work a session left running in the background, oldest first
    pub open: Vec<Task>,
    // what each background id of the session was started for
    pub names: HashMap<String, String>,
}

// alive: the ids of the session's commands that run now
pub fn background_of(transcript: &Path, now: f64, alive: &HashSet<&str>) -> Background {
    let Ok(meta) = std::fs::metadata(transcript) else { return Background { open: Vec::new(), names: HashMap::new() } };
    let size = meta.len();
    let known = CACHE.lock().expect("background cache").get(transcript).cloned();
    // a transcript only grows; one that shrank was rewritten and is read anew
    let mut scan = known.filter(|s| s.offset <= size).unwrap_or_default();
    if scan.offset < size {
        if let Ok(buffer) = read_tail(transcript, size, size - scan.offset) {
            // only whole lines: one still being written is read next time
            let end = buffer.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            for line in String::from_utf8_lossy(&buffer[..end]).split('\n') {
                scan.line(line);
            }
            scan.offset += end as u64;
            CACHE.lock().expect("background cache").insert(transcript.to_path_buf(), scan.clone());
        }
    }
    let mut open: Vec<Task> = scan
        .open
        .into_iter()
        .filter(|(id, t)| if t.workflow { now - t.started < WORKFLOW_MAX_AGE_MS && t.expires.is_none_or(|e| e > now) } else { alive.contains(id.as_str()) })
        .map(|(_, t)| t)
        .collect();
    open.sort_by(|a, b| a.started.total_cmp(&b.started));
    Background { open, names: scan.names }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    // the record of a call's result is spelt tool_use_result here, as the private
    // check asks (tools/check_private.py): the scan reads no field of it by name

    fn scan(lines: &[&str]) -> Scan {
        let mut s = Scan::default();
        for l in lines {
            s.line(l);
        }
        s
    }

    const USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 99","description":"Launch the app","run_in_background":true}}]}}"#;
    const RESULT: &str = r#"{"type":"user","timestamp":"2026-10-29T13:14:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"Command running in background with ID: b1unpd24f. Output is being written to: /tmp/x"}]}}"#;
    const DONE: &str = r#"{"type":"queue-operation","content":"<task-notification>\n<task-id>b1unpd24f</task-id>\n<status>completed</status>\n</task-notification>"}"#;

    #[test]
    fn a_background_command_is_open_until_its_notification() {
        let open = scan(&[USE, RESULT]);
        let task = open.open.get("b1unpd24f").expect("open");
        assert_eq!(task.what, "Launch the app");
        assert!(!task.workflow);
        assert!(scan(&[USE, RESULT, DONE]).open.is_empty());
    }

    #[test]
    fn a_flag_sent_as_a_string_counts_as_claude_code_reads_it() {
        // a call recorded with the flag as the model sent it, a string: Claude
        // Code runs a shell call with "true" in the background, with "false" in
        // the foreground
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_19","name":"Bash","input":{"command":"pnpm dev","description":"Start the dev server","run_in_background":"true"}}]}}"#;
        let launch = r#"{"type":"user","timestamp":"2026-11-04T08:20:00.000Z","message":{"content":[{"tool_use_id":"toolu_19","type":"tool_result","content":"Command running in background with ID: bstr12345. Output is being written to: /tmp/x"}]}}"#;
        let s = scan(&[call, launch]);
        assert_eq!(s.open["bstr12345"].what, "Start the dev server");
        assert_eq!(s.names.get("bstr12345").map(String::as_str), Some("Start the dev server"));
        // in the foreground, a launch it prints is only quoted: it lends no name
        let foreground = call.replace(r#""run_in_background":"true""#, r#""run_in_background":"false""#);
        assert_eq!(scan(&[&foreground, launch]).open["bstr12345"].what, "");
    }

    #[test]
    fn a_monitor_ends_when_it_expires() {
        let monitor = r#"{"type":"user","timestamp":"2026-10-29T13:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Monitor started (task mon42, expires in 15m unless the source ends first)"}]}}"#;
        let s = scan(&[monitor]);
        let task = s.open.get("mon42").expect("monitor");
        assert_eq!(task.expires, Some(task.started + 15.0 * 60_000.0));
    }

    // a Workflow call and its result as Claude Code 2.1.309 writes them: the
    // task is "w…", only the run is "wf_…"
    const FLOW: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01ExampleFlowCall0000001","name":"Workflow","input":{"script":"export default async function () {}"}}]}}"#;
    const LAUNCH: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":"Workflow launched in background. Task ID: w2abcdefg\nSummary: Review the uncommitted shop-api importer changes: 4 area reviewers, 2-lens verification per finding, a final sweep\nTranscript dir: C:\\Users\\me\\.claude\\projects\\p\\s\\subagents\\workflows\\wf_0a1b2c3d-4e5\nScript file: C:\\Users\\me\\.claude\\projects\\p\\s\\workflows\\scripts\\example-review-wf_0a1b2c3d-4e5.js\n(Edit this file with Write/Edit and re-invoke Workflow with {scriptPath: \"C:\\Users\\me\\.claude\\projects\\p\\s\\workflows\\scripts\\example-review-wf_0a1b2c3d-4e5.js\"} to iterate.)\n\nYou will be notified when it completes.","is_error":false}]},"uuid":"u1","timestamp":"2026-10-30T11:00:00.000Z","tool_use_result":{"status":"async_launched","taskId":"w2abcdefg","taskType":"local_workflow","runId":"wf_0a1b2c3d-4e5"}}"#;
    const FINISHED: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-30T12:00:00.000Z","content":"<task-notification>\n<task-id>w2abcdefg</task-id>\n<tool-use-id>toolu_01ExampleFlowCall0000001</tool-use-id>\n<status>completed</status>\n<summary>Dynamic workflow \"Review the uncommitted…\" completed</summary>\n</task-notification>"}"#;

    #[test]
    fn a_workflow_runs_without_a_process_until_its_notification() {
        let path = std::env::temp_dir().join(format!("pulse-background-flow-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{FLOW}\n{LAUNCH}\n")).expect("write");
        let started = parse_time("2026-10-30T11:00:00.000Z").expect("time");
        // as inputs.rs asks: no process holds a workflow's output
        let none = HashSet::new();
        let open = background_of(&path, started + 3_600_000.0, &none).open;
        assert_eq!(open.len(), 1);
        assert!(open[0].workflow);
        assert_eq!(open[0].started, started);
        assert_eq!(open[0].what, "Review the uncommitted shop-api importer changes: 4 area re…");
        // a notification that never comes
        assert!(background_of(&path, started + WORKFLOW_MAX_AGE_MS + 1.0, &none).open.is_empty());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open");
        writeln!(file, "{FINISHED}").expect("append");
        drop(file);
        assert!(background_of(&path, started + 3_600_000.0, &none).open.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_result_that_only_quotes_a_launch_starts_no_workflow() {
        let grep = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_7","name":"Bash","input":{"command":"grep -h 'Task ID' ~/.claude/projects/p/*.jsonl"}}]}}"#;
        let quote = r#"{"type":"user","timestamp":"2026-11-03T22:00:00.000Z","message":{"content":[{"tool_use_id":"toolu_7","type":"tool_result","content":"{\"type\":\"tool_result\",\"content\":\"Workflow launched in background. Task ID: wzzzzzzzz\\nSummary: x\"}"}]}}"#;
        let s = scan(&[FLOW, grep, quote]);
        assert!(s.open.values().all(|t| !t.workflow));
        let path = std::env::temp_dir().join(format!("pulse-background-quote-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{grep}\n{quote}\n")).expect("write");
        let now = parse_time("2026-11-03T23:00:00.000Z").expect("time");
        assert!(background_of(&path, now, &HashSet::new()).open.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_workflow_is_named_by_its_summary_else_workflow() {
        let bare = r#"{"type":"user","timestamp":"2026-11-03T21:00:00.000Z","message":{"content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":"Workflow launched in background. Task ID: w3abcdefg"}]}}"#;
        assert_eq!(scan(&[FLOW, bare]).open["w3abcdefg"].what, "workflow");
        let quoted = r#"{"type":"user","timestamp":"2026-11-03T21:00:00.000Z","message":{"content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":[{"type":"text","text":"Workflow launched in background. Task ID: w4abcdefg\nSummary: Review the \"foo\" module\nTranscript dir: C:\\x"}]}]}}"#;
        assert_eq!(scan(&[FLOW, quoted]).open["w4abcdefg"].what, "Review the \"foo\" module");
    }

    // the next request, as Claude Code keeps it beside the messages: the
    // results of the call, copied as they were, with no time
    const COPY: &str = r#"{"type":"api-request-blob","sessionId":"s","hash":"00c0ffee","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":"Workflow launched in background. Task ID: w2abcdefg\nSummary: Review the uncommitted shop-api importer changes: 4 area reviewers, 2-lens verification per finding, a final sweep\nTranscript dir: C:\\Users\\me\\.claude\\projects\\p\\s\\subagents\\workflows\\wf_0a1b2c3d-4e5\n\nYou will be notified when it completes.","is_error":false},{"tool_use_id":"toolu_01ExampleOtherCall000002","type":"tool_result","content":"ok","is_error":false}]}}"#;

    #[test]
    fn a_copy_or_a_quote_of_a_launch_leaves_the_workflow_running() {
        let path = std::env::temp_dir().join(format!("pulse-background-copy-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{FLOW}\n{LAUNCH}\n")).expect("write");
        let started = parse_time("2026-10-30T11:00:00.000Z").expect("time");
        let none = HashSet::new();
        assert_eq!(background_of(&path, started + 60_000.0, &none).open.len(), 1);
        // then the next request's copy, and a grep that prints the launch
        let grep = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_8","name":"Bash","input":{"command":"grep -h 'Task ID' ~/.claude/projects/p/s.jsonl"}}]}}"#;
        let quote = r#"{"type":"user","timestamp":"2026-10-30T11:07:00.000Z","message":{"content":[{"tool_use_id":"toolu_8","type":"tool_result","content":"Workflow launched in background. Task ID: w2abcdefg\nSummary: x"}]}}"#;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open");
        writeln!(file, "{COPY}\n{grep}\n{quote}").expect("append");
        drop(file);
        let open = background_of(&path, started + 3_600_000.0, &none).open;
        let _ = std::fs::remove_file(&path);
        assert_eq!(open.len(), 1);
        assert!(open[0].workflow);
        assert_eq!(open[0].started, started);
        assert_eq!(open[0].what, "Review the uncommitted shop-api importer changes: 4 area re…");
        // a copy read before the line it copies
        let s = scan(&[FLOW, COPY, LAUNCH]);
        assert!(s.open["w2abcdefg"].workflow);
        assert_eq!(s.open["w2abcdefg"].started, started);
    }

    #[test]
    fn a_request_copy_restarts_no_command_and_ends_no_monitor() {
        let copy = r#"{"type":"api-request-blob","sessionId":"s","hash":"00c0ffee","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"Command running in background with ID: b1unpd24f. Output is being written to: /tmp/x","is_error":false}]}}"#;
        let s = scan(&[USE, RESULT, copy]);
        assert_eq!(s.open["b1unpd24f"].what, "Launch the app");
        assert_eq!(s.open["b1unpd24f"].started, parse_time("2026-10-29T13:14:00.000Z").expect("time"));
        // a monitor's event has no status: in a copy, the status after it is the next notification's
        let monitor = r#"{"type":"user","timestamp":"2026-11-01T19:58:00.000Z","message":{"content":[{"tool_use_id":"t5","type":"tool_result","content":"Monitor started (task mon42, expires in 10m unless the source ends first)"}]}}"#;
        let events = r#"{"type":"api-request-blob","sessionId":"s","hash":"00decade","message":{"role":"user","content":[{"type":"text","text":"<task-notification>\n<task-id>mon42</task-id>\n<summary>Monitor event: \"tests\"</summary>\n<event>RESULT: 12 tests, 12 passed</event>\n</task-notification>\n\n<task-notification>\n<task-id>b6abcdefg</task-id>\n<status>completed</status>\n</task-notification>"}]}}"#;
        assert!(scan(&[monitor, events]).open.contains_key("mon42"));
    }

    #[test]
    fn a_remote_workflow_runs_like_a_local_one() {
        // as Claude Code 2.1.309 words a launch in a remote session
        let remote = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":"Workflow launched in a remote CCR session. Task ID: r7abcdefg\nSession: https://claude.ai/code/session_x\nSummary: Port the importer to the new API\n\nThe workflow runs against a fresh clone of the pushed branch; phase progress is visible at the session URL, not in /workflows. You will be notified when it completes.","is_error":false}]},"timestamp":"2026-10-30T11:00:00.000Z"}"#;
        let s = scan(&[FLOW, remote]);
        assert!(s.open["r7abcdefg"].workflow);
        assert_eq!(s.open["r7abcdefg"].what, "Port the importer to the new API");
    }

    #[test]
    fn a_summary_that_says_expires_in_does_not_end_a_workflow() {
        let launch = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleFlowCall0000001","type":"tool_result","content":"Workflow launched in background. Task ID: w5abcdefg\nSummary: Renew every certificate that expires in 30 days\n\nYou will be notified when it completes.","is_error":false}]},"timestamp":"2026-10-30T11:00:00.000Z"}"#;
        assert_eq!(scan(&[FLOW, launch]).open["w5abcdefg"].expires, None);
    }

    #[test]
    fn a_launch_of_an_older_wording_by_its_own_call_is_a_workflow() {
        // launched by its own call
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"Workflow","input":{"scriptPath":"x.js"}}]}}"#;
        // in two parts, so that a session reading this file starts no workflow
        let workflow = concat!(
            r#"{"type":"user","timestamp":"2026-10-29T13:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"Workflow started in background. Task "#,
            r#"ID: wf_abc123\nSummary: x"}]}}"#
        );
        let stopped = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t4","name":"TaskStop","input":{"task_id":"wf_abc123"}}]}}"#;
        let s = scan(&[call, workflow]);
        assert!(s.open["wf_abc123"].workflow);
        assert_eq!(s.open["wf_abc123"].what, "x");
        assert!(scan(&[call, workflow, stopped]).open.is_empty());
    }

    // a session (shortened): a Read of notes that quote a
    // launch of the old form, in two parts as above, then a workflow of its own
    const BRIEF: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01ExampleReadCall0000003","name":"Read","input":{"file_path":"C:\\Users\\me\\notes\\plan.txt","offset":120,"limit":140}}]}}"#;
    const QUOTED: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleReadCall0000003","type":"tool_result","content":"134\t   * To check: a result that quotes 'Workflow … Task "#,
        r#"ID: wf_x' opens no workflow."}]},"timestamp":"2026-10-30T12:05:00.000Z","tool_use_result":{"type":"text"}}"#
    );
    const HARDEN: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01ExampleFlowCall0000004","name":"Workflow","input":{"script":"export const meta = {\n  name: 'example-flow',\n}"}}]}}"#;
    const HARDENING: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleFlowCall0000004","type":"tool_result","content":"Workflow launched in background. Task ID: w6abcdefg\nSummary: Tidy the merged shop-web fixes in 4 small areas (each reviewed, fixed if needed) and run a final review of the tree\nTranscript dir: C:\\Users\\me\\.claude\\projects\\p\\s\\subagents\\workflows\\wf_5f6a7b8c-9d0"}]},"timestamp":"2026-10-30T12:10:00.000Z","tool_use_result":{"status":"async_launched","taskId":"w6abcdefg","taskType":"local_workflow","runId":"wf_5f6a7b8c-9d0"}}"#;

    #[test]
    fn a_quote_of_a_launch_of_the_old_form_starts_no_workflow() {
        // a quote of the old form comes first and opens nothing; the workflow after it does
        let s = scan(&[BRIEF, QUOTED, HARDEN, HARDENING]);
        assert_eq!(s.open.len(), 1);
        assert!(s.open["w6abcdefg"].workflow);
        assert_eq!(s.open["w6abcdefg"].what, "Tidy the merged shop-web fixes in 4 small areas (each revie…");
        assert!(!s.names.contains_key("wf_x"));
    }

    #[test]
    fn a_shell_call_that_quotes_a_launch_lends_it_no_name() {
        // a grep that prints the launch of a command, a monitor and a workflow of another session
        let grep = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_14","name":"Bash","input":{"command":"grep -h 'ID: ' notes.md","description":"Find the launches"}}]}}"#;
        let command = r#"{"type":"user","timestamp":"2026-10-30T13:30:00.000Z","message":{"content":[{"tool_use_id":"toolu_14","type":"tool_result","content":"Command running in background with ID: Z1. Output is being written to: /tmp/x"}]}}"#;
        let watch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_15","name":"PowerShell","input":{"command":"Get-Content notes.md","description":"Read the notes"}}]}}"#;
        let monitor = r#"{"type":"user","timestamp":"2026-10-30T13:31:00.000Z","message":{"content":[{"tool_use_id":"toolu_15","type":"tool_result","content":"Monitor started (task Z2, expires in 5m unless the source ends first)"}]}}"#;
        let cat = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_16","name":"Bash","input":{"command":"cat notes.md","description":"Show the notes"}}]}}"#;
        let workflow = r#"{"type":"user","timestamp":"2026-10-30T13:32:00.000Z","message":{"content":[{"tool_use_id":"toolu_16","type":"tool_result","content":"Workflow launched in background. Task ID: Z3"}]}}"#;
        let s = scan(&[grep, command, watch, monitor, cat, workflow]);
        // opened, as before, but not named for the shell call that quotes them
        assert_eq!(s.open["Z1"].what, "");
        assert_eq!(s.open["Z2"].what, "");
        assert_eq!(s.names.get("Z1").map(String::as_str), Some(""));
        assert!(!s.open.contains_key("Z3") && !s.names.contains_key("Z3"));
        assert!(s.calls.is_empty() && s.shells.is_empty());
    }

    const MONITOR: &str = r#"{"type":"user","timestamp":"2026-11-01T19:58:00.000Z","message":{"content":[{"tool_use_id":"t5","type":"tool_result","content":"Monitor started (task mon42, expires in 10m unless the source ends first)"}]}}"#;

    #[test]
    fn a_notification_without_a_status_takes_none_from_the_next() {
        // a queued line as Claude Code 2.1.316 writes it (shortened), holding two
        // notifications as a request does: a monitor's event, then another task's end
        let queued = r#"{"isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>mon42</task-id>\n<summary>Monitor event: \"tests\"</summary>\n<event>RESULT: 12 tests, 12 passed, 0 failed</event>\n</task-notification>\n\n<task-notification>\n<task-id>b6abcdefg</task-id>\n<status>completed</status>\n</task-notification>","commandMode":"task-notification"},"type":"attachment","timestamp":"2026-11-01T20:02:00.000Z"}"#;
        let s = scan(&[MONITOR, queued]);
        assert!(s.open.contains_key("mon42"));
        assert!(s.ended.contains("b6abcdefg"));
        // and one cut short (a queued prompt that pastes the start of one): the next is still read
        let cut = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-01T20:07:00.000Z","content":"<task-notification>\n<task-id>mon42</task-id>\n<summ\n<task-notification>\n<task-id>b6abcdefg</task-id>\n<status>completed</status>\n</task-notification>"}"#;
        let s = scan(&[MONITOR, cut]);
        assert!(s.open.contains_key("mon42"));
        assert!(s.ended.contains("b6abcdefg"));
    }

    #[test]
    fn a_result_that_quotes_an_end_notification_leaves_an_open_id_open() {
        // a grep of a transcript: the monitor's event, then another task's end
        let grep = r#"{"type":"user","timestamp":"2026-11-01T20:05:00.000Z","message":{"content":[{"tool_use_id":"toolu_9","type":"tool_result","content":"{\"type\":\"queue-operation\",\"content\":\"<task-notification>\\n<task-id>mon42</task-id>\\n<summary>Monitor event: \\\"tests\\\"</summary>\\n</task-notification>\"}\n{\"type\":\"queue-operation\",\"content\":\"<task-notification>\\n<task-id>b6abcdefg</task-id>\\n<status>completed</status>\\n</task-notification>\"}"}]}}"#;
        // a note that tells of a command's end notification
        let note = r#"{"type":"user","timestamp":"2026-11-01T20:06:00.000Z","message":{"content":[{"tool_use_id":"toolu_10","type":"tool_result","content":"The finished command b1unpd24f got a <\\task-notification> with <task-id>b1unpd24f</task-id> and <status>completed</status> (queue-operation enqueue)."}]}}"#;
        // and the event cut short, as a grep that prints only the start of a line does
        let cut = r#"{"type":"user","timestamp":"2026-11-01T20:07:00.000Z","message":{"content":[{"tool_use_id":"toolu_12","type":"tool_result","content":"<task-notification>\\n<task-id>mon42</task-id>\\n<summ\n<task-notification>\\n<task-id>b6abcdefg</task-id>\\n<status>completed</status>\\n</task-notification>"}]}}"#;
        // and a whole end notification, verbatim: printed by a result, written by a call
        let verbatim = r#"{"type":"user","timestamp":"2026-11-01T20:08:00.000Z","message":{"content":[{"tool_use_id":"toolu_17","type":"tool_result","content":"<task-notification>\n<task-id>b1unpd24f</task-id>\n<status>completed</status>\n</task-notification>"}]}}"#;
        let write = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_18","name":"Write","input":{"file_path":"note.md","content":"<task-notification>\n<task-id>mon42</task-id>\n<status>completed</status>\n</task-notification>"}}]}}"#;
        let s = scan(&[USE, RESULT, MONITOR, note, grep, cut, verbatim, write]);
        assert!(s.open.contains_key("b1unpd24f"));
        assert!(s.open.contains_key("mon42"));
        // the notification itself, handed on as a user's message, ends it
        let handed = r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>b1unpd24f</task-id>\n<status>completed</status>\n</task-notification>"},"origin":{"kind":"task-notification"},"timestamp":"2026-11-01T20:09:00.000Z"}"#;
        assert!(!scan(&[USE, RESULT, verbatim, handed]).open.contains_key("b1unpd24f"));
    }

    // a shell call that outran its timeout, and its result, as Claude Code 2.1.316 writes them (shortened)
    const SLOW: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01ExampleBashCall0000005","name":"Bash","input":{"command":"make slow-test","description":"Run the slow test suite"}}]}}"#;
    const MOVED: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01ExampleBashCall0000005","type":"tool_result","content":"Command did not complete within its 60s timeout and was moved to the background (ID: b8abcdefg). Output is being written to: C:\\Users\\me\\AppData\\Local\\Temp\\claude\\C--Users-me\\00000000-0000-4000-8000-000000000001\\tasks\\b8abcdefg.output. You will be notified when it completes. It is running now: do not start a second copy while it runs. If it is no longer needed, stop it with TaskStop. To check interim output, use Read on that file path.\nSession cwd remains C:\\Users\\me; directory changes made by the backgrounded command do not apply to subsequent commands.","is_error":false}]},"timestamp":"2026-11-02T14:00:00.000Z","tool_use_result":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"b8abcdefg","timedOutAfterMs":60000}}"#;

    #[test]
    fn a_command_moved_to_the_background_by_its_timeout_is_named_not_opened() {
        let s = scan(&[SLOW, MOVED]);
        assert_eq!(s.names.get("b8abcdefg").map(String::as_str), Some("Run the slow test suite"));
        assert!(s.open.is_empty());
        // a grep that prints the result names nothing, and keeps no call
        let grep = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_11","name":"Bash","input":{"command":"grep -h 'moved to the background' ~/.claude/projects/p/s.jsonl","description":"Find timed-out commands"}}]}}"#;
        let quote = r#"{"type":"user","timestamp":"2026-11-02T15:00:00.000Z","message":{"content":[{"tool_use_id":"toolu_11","type":"tool_result","content":"{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"Command did not complete within its 60s timeout and was moved to the background (ID: b8abcdefg).\"}]},\"toolUseResult\":{\"backgroundTaskId\":\"b8abcdefg\",\"timedOutAfterMs\":60000}}"}]},"tool_use_result":{"stdout":"{\"toolUseResult\":{\"backgroundTaskId\":\"b8abcdefg\"}}","stderr":""}}"#;
        let s = scan(&[grep, quote]);
        assert!(s.names.is_empty());
        assert!(s.calls.is_empty() && s.shells.is_empty());
        // a record that names an id its text does not say was moved (a tool's
        // structured result that quotes the field) names nothing
        let record = r#"{"type":"user","timestamp":"2026-11-02T15:05:00.000Z","message":{"content":[{"tool_use_id":"toolu_13","type":"tool_result","content":"1 command moved"}]},"tool_use_result":{"structuredContent":{"backgroundTaskId":"b8abcdefg"}}}"#;
        assert!(scan(&[record]).names.is_empty());
    }

    // a shell call the user sent to the background by hand (Ctrl+B), and its
    // result, in the shape Claude Code 2.1.317 gives them
    const BUILD: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01RkH5wqNcJ3mXv8TbLp2ZsA","name":"Bash","input":{"command":"npm run build","description":"Build the app"}}]}}"#;
    const BY_HAND: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01RkH5wqNcJ3mXv8TbLp2ZsA","type":"tool_result","content":"Command was manually backgrounded by user with ID: b4hn2xk7q. Output is being written to: C:\\Users\\me\\AppData\\Local\\Temp\\claude\\C--Users-me\\00000000-0000-4000-8000-000000000001\\tasks\\b4hn2xk7q.output.","is_error":false}]},"timestamp":"2026-11-04T09:00:00.000Z","tool_use_result":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"b4hn2xk7q","backgroundedByUser":true}}"#;

    #[test]
    fn a_command_the_user_moved_to_the_background_is_named_not_opened() {
        let s = scan(&[BUILD, BY_HAND]);
        assert_eq!(s.names.get("b4hn2xk7q").map(String::as_str), Some("Build the app"));
        assert!(s.open.is_empty());
        assert!(s.calls.is_empty() && s.shells.is_empty());
    }

    #[test]
    fn a_record_whose_id_starts_or_runs_past_the_moved_one_names_nothing() {
        // the text gives the id to its end, a ')' after a timeout, a '.' by
        // hand: a record of only its start, or of more, names nothing
        for (call, moved, id) in [(SLOW, MOVED, "b8abcdefg"), (BUILD, BY_HAND, "b4hn2xk7q")] {
            for other in [id[..4].to_string(), format!("{id}9")] {
                let record = moved.replace(&format!(r#""backgroundTaskId":"{id}""#), &format!(r#""backgroundTaskId":"{other}""#));
                let s = scan(&[call, &record]);
                assert!(s.names.is_empty() && s.open.is_empty(), "{other}");
            }
        }
    }
}
