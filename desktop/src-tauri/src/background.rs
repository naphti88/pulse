// The background work of a session that goes on after its turn ended: a
// command run in the background, a workflow, a monitor. Each gets an id when
// it starts ("running in background with ID: …", "Task ID: …", "Monitor
// started (task …"); a <task-notification> with that id and a final status
// ends it. A session with such work open is still running: it wakes up when
// the work is done. A command or monitor runs as long as its process does (the
// process table says so, src/procs.rs); a workflow has none and counts by the
// transcript alone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::rules::limits::WORKFLOW_MAX_AGE_MS;
use crate::util::{parse_time, read_tail};

#[derive(Clone)]
pub struct Task {
    pub what: String,
    pub started: f64,
    // a monitor ends on its own after its time
    pub expires: Option<f64>,
}

#[derive(Clone, Default)]
struct Scan {
    // how far the transcript was read: the end of its last whole line
    offset: u64,
    open: HashMap<String, Task>,
    // the description of a background call, by its tool use id, until its result names the id
    calls: HashMap<String, String>,
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

impl Scan {
    fn line(&mut self, line: &str) {
        // a background call: remember what it is for, by its tool use id
        if line.contains("\"tool_use\"") && (line.contains("\"run_in_background\":true") || line.contains("\"name\":\"Monitor\"") || line.contains("\"name\":\"Workflow\"")) {
            if let Ok(entry) = serde_json::from_str::<Value>(line) {
                let blocks = entry.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
                for b in blocks.into_iter().flatten().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")) {
                    let input = b.get("input");
                    let text = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
                    let what = text("description").or(text("command")).or(text("scriptPath")).or(text("name")).unwrap_or("");
                    if let Some(id) = b.get("id").and_then(Value::as_str) {
                        self.calls.insert(id.to_string(), short(what));
                    }
                }
            }
        }
        // its result names the id it runs under
        if line.contains("\"tool_result\"") {
            let id = id_after(line, "running in background with ID: ")
                .or_else(|| if line.contains("Workflow") { id_after(line, "Task ID: ") } else { None })
                .or_else(|| id_after(line, "Monitor started (task "));
            if let Some(id) = id.filter(|id| !self.ended.contains(*id)) {
                let started = field(line, "timestamp").and_then(parse_time).unwrap_or(0.0);
                let expires = id_after(line, "expires in ").and_then(|m| m.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok()).map(|m| started + m * 60_000.0);
                let what = field(line, "tool_use_id").and_then(|u| self.calls.remove(u)).unwrap_or_default();
                self.names.insert(id.to_string(), what.clone());
                self.open.insert(id.to_string(), Task { what, started, expires });
            }
        }
        // a notification ends it, as does stopping it
        let mut rest = line;
        while let Some(at) = rest.find("<task-id>") {
            rest = &rest[at..];
            if let Some(id) = id_after(rest, "<task-id>") {
                let status = rest.find("<status>").map(|s| &rest[s + 8..]).and_then(|s| s.split('<').next()).unwrap_or("");
                if !matches!(status, "running" | "pending" | "") {
                    self.open.remove(id);
                    self.ended.insert(id.to_string());
                }
            }
            rest = &rest[9..];
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

// a workflow's id; a command's or a monitor's is that of its process' output
fn is_workflow(id: &str) -> bool {
    id.starts_with("wf_")
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
        .filter(|(id, t)| if is_workflow(id) { now - t.started < WORKFLOW_MAX_AGE_MS && t.expires.is_none_or(|e| e > now) } else { alive.contains(id.as_str()) })
        .map(|(_, t)| t)
        .collect();
    open.sort_by(|a, b| a.started.total_cmp(&b.started));
    Background { open, names: scan.names }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(scan(&[USE, RESULT, DONE]).open.is_empty());
    }

    #[test]
    fn a_monitor_ends_when_it_expires_and_a_workflow_by_its_id() {
        let monitor = r#"{"type":"user","timestamp":"2026-10-29T13:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Monitor started (task mon42, expires in 15m unless the source ends first)"}]}}"#;
        let s = scan(&[monitor]);
        let task = s.open.get("mon42").expect("monitor");
        assert_eq!(task.expires, Some(task.started + 15.0 * 60_000.0));
        let workflow = r#"{"type":"user","timestamp":"2026-10-29T13:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"Workflow started in background. Task ID: wf_abc123\nSummary: x"}]}}"#;
        let stopped = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t4","name":"TaskStop","input":{"task_id":"wf_abc123"}}]}}"#;
        assert!(scan(&[workflow]).open.contains_key("wf_abc123"));
        assert!(scan(&[workflow, stopped]).open.is_empty());
    }
}
