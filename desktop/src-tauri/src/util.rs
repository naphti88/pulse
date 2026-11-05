// Where the app and the routine-recorder keep their files, and the small
// helpers every reader shares.

use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set.".to_string())
}

pub struct Paths {
    pub app: PathBuf,
    pub recorder: PathBuf,
    pub runs: PathBuf,
    pub pending: PathBuf,
    pub projects: PathBuf,
    pub leveldb: PathBuf,
}

pub fn paths() -> Result<Paths, String> {
    let home = home()?;
    Ok(Paths {
        app: home.join("Library/Application Support/Claude/claude-code-sessions"),
        recorder: home.join(".claude/routine-recorder"),
        runs: home.join(".claude/routine-recorder/runs"),
        pending: home.join(".claude/routine-recorder/pending"),
        projects: home.join(".claude/projects"),
        leveldb: home.join("Library/Application Support/Claude/Local Storage/leveldb"),
    })
}

pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

pub fn mtime_ms(meta: &Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// an ISO timestamp as the transcripts write it, in ms like Date.parse
pub fn parse_time(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis() as f64)
}

pub fn kind(line: &Value) -> Option<&str> {
    line.get("type").and_then(Value::as_str).filter(|t| !t.is_empty())
}

pub fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub fn content(line: &Value) -> Option<&Value> {
    line.get("message").and_then(|m| m.get("content"))
}

// the text blocks of an entry, joined by new lines
pub fn text_blocks(line: &Value) -> String {
    match content(line) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// whether an assistant entry hands a subagent's report back
pub fn is_handback(line: Option<&Value>) -> bool {
    let Some(line) = line else { return false };
    kind(line) == Some("assistant")
        && content(line).and_then(Value::as_array).is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_use")
                    && b.get("name").and_then(Value::as_str) == Some("SubagentHandback")
            })
        })
}

// the JSON lines of a text; lines that do not parse (one still being
// written) are left out
pub fn json_lines(text: &str, skip_first: bool) -> Vec<Value> {
    text.split('\n')
        .skip(usize::from(skip_first))
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// the last `length` bytes of a file
pub fn read_tail(path: &Path, size: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(size - length))?;
    let mut buffer = vec![0; length as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

// the first `length` bytes of a file, fewer if it is shorter
pub fn read_head(path: &Path, length: u64) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    File::open(path)?.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

// the first n characters, as String.slice(0, n)
pub fn first_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}
