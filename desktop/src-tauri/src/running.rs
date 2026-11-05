// Whether a chat is in the middle of a turn, read from the end of its
// transcript. The app keeps writing to a transcript after a turn ended
// (cost, title, summary), so the file's time says nothing; its last real
// entry does: a turn ends with the answer's end_turn and a stop_hook_summary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::rules::limits::TURN_STALE_MS;
use crate::util::{content, flag, is_handback, json_lines, kind, mtime_ms, parse_time, read_tail};

// read the end of the file, more of it while no whole entry fits (one tool
// result can be larger than the first window)
const TAIL_STEPS: [u64; 3] = [64 * 1024, 512 * 1024, 2 * 1024 * 1024];
// the entries of the conversation itself; the app writes more kinds beside
// them (cost, title, mode, a PR link, an API request …) and adds new ones, which
// say nothing about whether a turn is open
const CONVERSATION: [&str; 3] = ["user", "assistant", "system"];

// every text an entry holds, joined without a separator
fn all_text(line: &Value) -> String {
    match content(line) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("text") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            })
            .collect(),
        _ => String::new(),
    }
}

// a real entry: of the conversation, and in a session's transcript not a sidechain
fn counts(line: &Value, is_subagent: bool) -> bool {
    kind(line).is_some_and(|t| CONVERSATION.contains(&t)) && !(flag(line, "isSidechain") && !is_subagent)
}

// the last entries of a transcript, newest last → is a turn open?
// a subagent's own transcript is all sidechain: count those lines there
pub fn is_turn_open(lines: &[Value], now: f64, is_subagent: bool) -> bool {
    let real: Vec<&Value> = lines.iter().filter(|l| counts(l, is_subagent)).collect();
    // the handback, or the result that confirms it
    if is_subagent {
        let last = real.last().copied();
        let before = real.len().checked_sub(2).map(|i| real[i]);
        if is_handback(last) || (last.and_then(kind) == Some("user") && is_handback(before)) {
            return false;
        }
    }
    for line in lines.iter().rev() {
        if !counts(line, is_subagent) {
            continue;
        }
        match kind(line) {
            Some("system") => {
                let subtype = line.get("subtype").and_then(Value::as_str);
                if matches!(subtype, Some("stop_hook_summary" | "api_error" | "turn_duration")) {
                    return false;
                }
                continue;
            }
            Some("assistant")
                if line.get("message").and_then(|m| m.get("stop_reason")).and_then(Value::as_str) == Some("end_turn") =>
            {
                return false
            }
            Some("user") if all_text(line).starts_with("[Request interrupted") => return false,
            _ => {}
        }
        // a prompt, a tool call or its result: the turn goes on, unless it went quiet long ago
        let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
        return at.is_some_and(|at| now - at < TURN_STALE_MS);
    }
    false
}

struct Hit {
    mtime_ms: f64,
    open: bool,
    checked_at: f64,
}

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Hit>>> = LazyLock::new(Default::default);

fn read_open(path: &Path, size: u64, now: f64, is_subagent: bool) -> std::io::Result<bool> {
    let mut lines = Vec::new();
    for step in TAIL_STEPS {
        let length = size.min(step);
        let buffer = read_tail(path, size, length)?;
        // the first line of a cut tail is partial
        lines = json_lines(&String::from_utf8_lossy(&buffer), size > length);
        if length == size || lines.iter().any(|l| kind(l).is_some_and(|t| CONVERSATION.contains(&t))) {
            break;
        }
    }
    Ok(is_turn_open(&lines, now, is_subagent))
}

pub fn is_running(path: &Path, now: f64, is_subagent: bool) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    let mtime = mtime_ms(&meta);
    {
        let cache = CACHE.lock().expect("running cache");
        // unchanged file: the answer only changes when a turn goes stale
        if let Some(hit) = cache.get(path) {
            if hit.mtime_ms == mtime && (!hit.open || now - hit.checked_at < 60_000.0) {
                return hit.open;
            }
        }
    }
    let Ok(open) = read_open(path, meta.len(), now, is_subagent) else { return false };
    CACHE.lock().expect("running cache").insert(path.to_path_buf(), Hit { mtime_ms: mtime, open, checked_at: now });
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::now_ms;

    fn lines(list: &[String]) -> Vec<Value> {
        list.iter().map(|l| serde_json::from_str(l).expect("json")).collect()
    }

    #[test]
    fn what_the_app_writes_after_a_turn_does_not_open_it_again() {
        let now = now_ms();
        let at = chrono::DateTime::from_timestamp_millis((now - 60_000.0) as i64).expect("time").to_rfc3339();
        let answer = format!(r#"{{"type":"assistant","timestamp":"{at}","message":{{"stop_reason":"end_turn","content":[{{"type":"text","text":"Fertig."}}]}}}}"#);
        let stop = format!(r#"{{"type":"system","subtype":"stop_hook_summary","timestamp":"{at}"}}"#);
        let pr = format!(r#"{{"type":"pr-link","timestamp":"{at}"}}"#);
        let call = format!(r#"{{"type":"assistant","timestamp":"{at}","message":{{"stop_reason":"tool_use","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{}}}}]}}}}"#);
        assert!(!is_turn_open(&lines(&[answer.clone(), stop.clone(), pr.clone()]), now, false));
        assert!(is_turn_open(&lines(&[answer, stop, call, pr]), now, false));
    }
}
