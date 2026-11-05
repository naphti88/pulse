// What a chat said of its last answer: the STATUS line the recorder asks
// every session for, "STATUS: worked | Import geladen.", from the end
// of its transcript. Only what came after the last prompt counts: a new
// prompt the answer has not yet closed says nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::live::is_prompt;
use crate::rules::Word;
use crate::util::{flag, json_lines, kind, read_tail, text_blocks};

#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub state: Word,
    pub text: String,
}

// a markdown reference the chat does not show, "[//]: # (", and its end
fn reference(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("[//]:")?.trim_start().strip_prefix('#')?.trim_start().strip_prefix('(')?;
    Some(rest.trim_end().strip_suffix(')').unwrap_or(rest))
}

// one line of an answer: as asked for, as an HTML comment, as a plain line
// or set in markdown
fn status_line(line: &str) -> Option<Said> {
    let line = line.trim_start_matches([' ', '\t', '>', '*', '_', '`']);
    let line = reference(line).or_else(|| line.strip_prefix("<!--")).unwrap_or(line).trim_start();
    let rest = line.strip_prefix("STATUS:")?.trim_start();
    let (name, state) = Word::ALL.into_iter().find(|(name, _)| {
        rest.get(..name.len()).is_some_and(|word| word.eq_ignore_ascii_case(name)) && !rest[name.len()..].starts_with(|c: char| c.is_alphanumeric())
    })?;
    let text = rest[name.len()..].trim_end_matches([' ', '\t', '*', '_', '`']);
    let text = text.strip_suffix("-->").unwrap_or(text);
    let text = text.trim_start().trim_start_matches(['|', ':', '–', '-']).trim();
    Some(Said { state, text: text.to_string() })
}

// the last STATUS line of an answer
pub fn status_of(text: &str) -> Option<Said> {
    text.lines().filter_map(status_line).last()
}

fn apply(said: &mut Option<Said>, line: &Value) {
    if is_prompt(line) {
        *said = None;
    } else if kind(line) == Some("assistant") && !flag(line, "isSidechain") {
        if let Some(found) = status_of(&text_blocks(line)) {
            *said = Some(found);
        }
    }
}

// by transcript: its size when it was read and what that gave
static CACHE: LazyLock<Mutex<HashMap<PathBuf, (u64, Option<Said>)>>> = LazyLock::new(Default::default);

// the end of the transcript holds the last answer and what follows it
const TAIL: u64 = 256 * 1024;

pub fn said_of(transcript: &Path) -> Option<Said> {
    let size = std::fs::metadata(transcript).ok()?.len();
    if let Some((known, said)) = CACHE.lock().expect("said cache").get(transcript) {
        if *known == size {
            return said.clone();
        }
    }
    let length = size.min(TAIL);
    let buffer = read_tail(transcript, size, length).ok()?;
    let text = String::from_utf8_lossy(&buffer);
    let mut said = None;
    // a cut first line is partial
    for line in json_lines(&text, length < size) {
        apply(&mut said, &line);
    }
    CACHE.lock().expect("said cache").insert(transcript.to_path_buf(), (size, said.clone()));
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(json: &str) -> Value {
        serde_json::from_str(json).expect("json")
    }

    fn answer(text: &str) -> Value {
        line(&serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}).to_string())
    }

    const PROMPT: &str = r#"{"type":"user","message":{"content":"Weiter"}}"#;

    #[test]
    fn the_comment_a_plain_line_and_markdown() {
        let said = |state: &str, text: &str| Some(Said { state: state.into(), text: text.into() });
        assert_eq!(status_of("Fertig.\n\n[//]: # (STATUS: worked | Import geladen.)"), said("worked", "Import geladen."));
        assert_eq!(status_of("[//]: # (STATUS: waiting | Satz mit Klammer\\))"), said("waiting", "Satz mit Klammer\\)"));
        assert_eq!(status_of("Fertig.\n<!-- STATUS: worked | Import geladen. -->"), said("worked", "Import geladen."));
        assert_eq!(status_of("STATUS: waiting | Zwei Punkte offen."), said("waiting", "Zwei Punkte offen."));
        assert_eq!(status_of("**STATUS: blocked – SSH abgelehnt.**"), said("blocked", "SSH abgelehnt."));
        assert_eq!(status_of("<!--STATUS:idle-->"), said("idle", ""));
        assert_eq!(status_of("STATUS: <idle|worked|waiting|blocked> | Satz"), None);
        assert_eq!(status_of("Der STATUS: worked steht im Satz"), None);
    }

    #[test]
    fn only_the_answer_after_the_last_prompt_counts() {
        let mut said = None;
        apply(&mut said, &answer("Teil 1 fertig.\n\n[//]: # (STATUS: waiting | Teil 2 offen.)"));
        assert_eq!(said.as_ref().map(|s| s.state.as_str()), Some("waiting"));
        apply(&mut said, &line(PROMPT));
        assert_eq!(said, None);
        apply(&mut said, &answer("Ich schaue nach."));
        assert_eq!(said, None);
        apply(&mut said, &answer("Alles erledigt.\n<!-- STATUS: worked | Teil 2 fertig. -->"));
        assert_eq!(said.map(|s| s.state), Some("worked".into()));
    }
}
