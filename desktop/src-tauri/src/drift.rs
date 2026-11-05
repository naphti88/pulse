// Pulse reads files of the Claude app that are no documented interface, and an
// update can change them. Most changes would not fail, they would quietly leave
// a field empty and every item a little wrong. This looks at what came in and
// says so in the panel's foot. It only speaks with enough to go on: one odd
// session is no change of format, and a kind of entry nobody knows is normal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::inputs::Session;
use crate::rules::limits::ACTIVE_MS;
use crate::util::{json_lines, kind, parse_time, read_tail};

// fewer than this says nothing
const ENOUGH: usize = 5;
const TAIL: u64 = 256 * 1024;

fn share_missing(sessions: &[&Session], has: impl Fn(&Session) -> bool) -> bool {
    sessions.len() >= ENOUGH && sessions.iter().filter(|s| has(s)).count() * 2 < sessions.len()
}

// what every session of the app has had so far; found: how many of the active
// ones have a transcript, said: how many said a STATUS
pub fn sessions(sessions: &[Session], now: f64, found: usize, said: usize) -> Vec<String> {
    let active: Vec<&Session> = sessions.iter().filter(|s| !s.archived && s.last_activity.unwrap_or(0.0) > now - ACTIVE_MS).collect();
    let all: Vec<&Session> = sessions.iter().collect();
    let mut problems = Vec::new();
    let mut gone = |what: &str| problems.push(format!("The Claude app changed its session files: no {what}. Pulse needs an update."));
    if share_missing(&all, |s| s.cli_id.is_some()) {
        gone("session id for the transcript");
    }
    if share_missing(&all, |s| s.last_activity.is_some()) {
        gone("last activity");
    }
    if share_missing(&all, |s| s.cwd.is_some()) {
        gone("folder");
    }
    if share_missing(&all, |s| s.summary.category.is_some()) {
        gone("summary after a turn");
    }
    let with_id = active.iter().filter(|s| s.cli_id.is_some()).count();
    if with_id >= ENOUGH && found * 2 < with_id {
        problems.push("Transcripts are not where they used to be. Pulse needs an update.".to_string());
    }
    if found >= ENOUGH && said == 0 {
        problems.push("No recent session ended with a STATUS line. Is the routine-recorder plugin installed?".to_string());
    }
    problems
}

// the shape of a transcript's entries, what the rules lean on
pub fn transcript(lines: &[Value]) -> Vec<String> {
    let of = |name: &str| lines.iter().filter(|l| kind(l) == Some(name)).collect::<Vec<_>>();
    let (assistant, user) = (of("assistant"), of("user"));
    if assistant.len() < ENOUGH {
        return Vec::new();
    }
    let mut changed = Vec::new();
    let timed = assistant.iter().chain(&user).filter(|l| l.get("timestamp").and_then(Value::as_str).and_then(parse_time).is_some()).count();
    if timed * 2 < assistant.len() + user.len() {
        changed.push("times");
    }
    if !assistant.iter().any(|l| l.get("message").and_then(|m| m.get("content")).is_some_and(Value::is_array)) {
        changed.push("the content of an answer");
    }
    if !assistant.iter().any(|l| l.get("message").and_then(|m| m.get("stop_reason")).is_some_and(Value::is_string)) {
        changed.push("how a turn ends");
    }
    let blocks = assistant.iter().filter_map(|l| l.get("message")?.get("content")?.as_array()).flatten();
    let calls: Vec<&Value> = blocks.filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")).collect();
    if !calls.is_empty() && !calls.iter().any(|b| b.get("id").is_some() && b.get("name").is_some()) {
        changed.push("the calls of tools");
    }
    if changed.is_empty() {
        Vec::new()
    } else {
        vec![format!("Claude's transcripts changed: {}. Pulse needs an update.", changed.join(", "))]
    }
}

// by transcript: its size when it was looked at, and what that gave
static LOOKED_AT: LazyLock<Mutex<HashMap<PathBuf, (u64, Vec<String>)>>> = LazyLock::new(Default::default);

// the newest transcript, once per size; the tail of a growing file is enough
pub fn transcript_file(path: &Path) -> Vec<String> {
    let Ok(meta) = std::fs::metadata(path) else { return Vec::new() };
    let size = meta.len();
    if let Some((known, problems)) = LOOKED_AT.lock().expect("drift cache").get(path) {
        if *known == size {
            return problems.clone();
        }
    }
    let length = size.min(TAIL);
    let Ok(buffer) = read_tail(path, size, length) else { return Vec::new() };
    let problems = transcript(&json_lines(&String::from_utf8_lossy(&buffer), length < size));
    let mut cache = LOOKED_AT.lock().expect("drift cache");
    cache.clear();
    cache.insert(path.to_path_buf(), (size, problems.clone()));
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::Summary;
    use serde_json::json;

    const NOW: f64 = 1_792_000_000_000.0;

    fn session(id: usize) -> Session {
        Session {
            id: format!("s{id}"),
            cli_id: Some(format!("c{id}")),
            cwd: Some("/Users/x/Work/shop-api".into()),
            last_activity: Some(NOW - 1000.0),
            summary: Summary { category: Some("completed".into()), ..Summary::default() },
            ..Session::default()
        }
    }

    #[test]
    fn a_field_gone_from_most_sessions_is_told_and_one_odd_session_is_not() {
        let mut list: Vec<Session> = (0..6).map(session).collect();
        assert!(sessions(&list, NOW, 6, 6).is_empty());
        list[0].cli_id = None;
        assert!(sessions(&list, NOW, 5, 5).is_empty());
        for s in &mut list {
            s.cli_id = None;
        }
        assert_eq!(sessions(&list, NOW, 0, 0).len(), 1);
        // too few to say anything
        assert!(sessions(&list[..3], NOW, 0, 0).is_empty());
    }

    #[test]
    fn transcripts_that_moved_and_a_status_line_nobody_writes_are_told() {
        let list: Vec<Session> = (0..6).map(session).collect();
        assert!(sessions(&list, NOW, 1, 1)[0].contains("Transcripts"));
        assert!(sessions(&list, NOW, 6, 0)[0].contains("plugin"));
    }

    fn answer(stop: Value) -> Value {
        json!({"type": "assistant", "timestamp": "2026-10-30T10:00:00.000Z", "message": {"stop_reason": stop, "content": [{"type": "tool_use", "id": "t", "name": "Bash"}]}})
    }

    #[test]
    fn the_shape_of_a_transcript_is_told_only_when_it_changed() {
        let good = vec![answer(json!("end_turn")); 6];
        assert!(transcript(&good).is_empty());
        // a kind of entry nobody knows changes nothing, too few say nothing
        let mut more = good.clone();
        more.push(json!({"type": "something-new"}));
        assert!(transcript(&more).is_empty());
        assert!(transcript(&good[..2]).is_empty());
        let no_end = vec![answer(Value::Null); 6];
        assert!(transcript(&no_end)[0].contains("how a turn ends"));
        let renamed = vec![json!({"type": "assistant", "time": "x", "msg": {}}); 6];
        let told = &transcript(&renamed)[0];
        assert!(told.contains("times") && told.contains("the content of an answer"), "{told}");
    }

    #[test]
    fn the_cases_of_real_sessions_have_the_shape_that_is_expected() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/background_command_alive.json");
        let case: Value = serde_json::from_slice(&std::fs::read(dir).expect("case")).expect("json");
        let lines = case["lines"].as_array().expect("lines");
        assert!(lines.len() > ENOUGH);
        assert!(transcript(lines).is_empty());
    }
}
