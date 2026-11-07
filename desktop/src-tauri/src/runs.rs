// What a routine run came to: the recorder's record of its turns, joined with
// the app's session, gives one state and the words that sum it up.

use std::collections::HashMap;

use serde::Deserialize;

use crate::items::State;
use crate::rules::limits::{LIVE_MS, QUIET_RUN_CHARS, QUIET_RUN_MS, QUIET_RUN_TOOLS};
use crate::rules::Word;
use crate::said::bare;

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct Permission {
    pub outcome: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct Turn {
    pub by: String,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub status: Option<String>,
    pub status_text: Option<String>,
    pub answer_tail: Option<String>,
    pub tools: HashMap<String, f64>,
    pub permissions: Vec<Permission>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct RunRecord {
    pub session_id: String,
    pub task_id: String,
    pub started_at: f64,
    pub updated_at: f64,
    pub active_turn_id: Option<String>,
    pub turns: Vec<Turn>,
}

// the app's own summary after a turn
#[derive(Default, Clone, Debug)]
pub struct Summary {
    pub category: Option<String>,
    pub detail: String,
    pub needs_action: String,
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in chars.windows(2) {
        let ((_, a), (j, b)) = (w[0], w[1]);
        if matches!(a, '.' | '!' | '?') && b.is_whitespace() {
            out.push(&text[start..j]);
            start = j;
        }
    }
    out.push(&text[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

// the closing words of an answer, from the tail the recorder keeps: code
// blocks are skipped, and of a long last paragraph only its closing
// sentences are kept, which is where a question to the person sits
pub fn last_line(text: Option<&str>) -> String {
    let text = text.unwrap_or("");
    let lines: Vec<&str> = text.split('\n').collect();
    // an odd count of fences means the tail starts inside a code block
    let mut in_fence = lines.iter().filter(|l| l.trim().starts_with("```")).count() % 2 == 1;
    let mut prose = Vec::new();
    for l in &lines {
        let t = l.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        // a STATUS or PING line, plain, set in markdown, or as a comment or reference
        let upper = bare(t).to_uppercase();
        if !in_fence && !t.is_empty() && !upper.starts_with("STATUS:") && !upper.starts_with("PING:") {
            prose.push(t);
        }
    }
    // a tail as long as the recorder keeps may start mid-sentence
    let is_cut = text.chars().count() >= 300 && prose.len() == 1;
    let line = prose.last().copied().unwrap_or("");
    if line.chars().count() <= 220 {
        return if is_cut { format!("… {line}") } else { line.to_string() };
    }
    let parts = sentences(line);
    let mut out = String::new();
    for s in parts.iter().rev() {
        let next = if out.is_empty() { s.to_string() } else { format!("{s} {out}") };
        if !out.is_empty() && next.chars().count() > 220 {
            break;
        }
        out = next;
    }
    if out == line {
        out
    } else {
        format!("… {out}")
    }
}

impl Summary {
    // the words a session is known by: what it said itself, else what the app
    // noted that it asks, else the app's one-line summary
    pub fn words(&self, said: Option<String>) -> String {
        match said.filter(|t| !t.is_empty()) {
            Some(said) => said,
            None if !self.needs_action.is_empty() => self.needs_action.clone(),
            None => self.detail.clone(),
        }
    }
}

// what the record of a run says, for the rules (rules.rs)
pub struct RunFacts {
    pub word: Option<Word>,
    pub status_text: Option<String>,
    pub closing: String,
    pub record_open: bool,
    // a permission refused or unanswered in the last round
    pub refused: bool,
    // so small that it found nothing
    pub quiet_shape: bool,
    pub started_at: f64,
    pub last_activity: f64,
}

pub fn run_facts(rec: Option<&RunRecord>, summary: &Summary, session_created: Option<f64>, session_activity: Option<f64>, now: f64) -> RunFacts {
    let turns: &[Turn] = rec.map(|r| r.turns.as_slice()).unwrap_or(&[]);
    let last = turns.last();
    let user_turns = turns.iter().filter(|t| t.by == "user").count();
    // a refusal the run got past is history; only one in the last round is
    // still open, and a run the app files as completed with no question left settled it too
    let is_settled = summary.category.as_deref() == Some("completed") && summary.needs_action.is_empty();
    let open = |outcome: &str| {
        if is_settled {
            return 0;
        }
        last.map(|t| t.permissions.iter().filter(|p| p.outcome.as_deref() == Some(outcome)).count()).unwrap_or(0)
    };
    let (open_denied, open_unanswered) = (open("denied"), open("unanswered"));
    let started_at = rec.map(|r| r.started_at).or(session_created).or(session_activity).unwrap_or(0.0);
    let last_activity = rec.map(|r| r.updated_at).unwrap_or(0.0).max(session_activity.unwrap_or(0.0)).max(started_at);
    let tail = last.and_then(|t| t.answer_tail.as_deref());
    let tools: f64 = turns.iter().map(|t| t.tools.values().sum::<f64>()).sum();
    let duration = last.and_then(|t| t.ended_at).unwrap_or(last_activity) - started_at;
    let tail_len = tail.map(|t| t.chars().count()).unwrap_or(0);
    RunFacts {
        word: last.and_then(|t| t.status.as_deref()).and_then(Word::parse),
        status_text: last.and_then(|t| t.status_text.clone()),
        closing: last_line(tail),
        record_open: rec.is_some_and(|r| r.active_turn_id.is_some() && r.updated_at > now - LIVE_MS),
        refused: open_denied + open_unanswered > 0,
        quiet_shape: user_turns == 0 && tools <= QUIET_RUN_TOOLS && tail_len <= QUIET_RUN_CHARS && duration < QUIET_RUN_MS && open_denied == 0,
        started_at,
        last_activity,
    }
}

impl RunFacts {
    // the words that sum a run up: its own STATUS sentence, or Claude's closing
    // line, in the person's language, before the app's English one-line summary
    pub fn reason(&self, state: State, summary: &Summary) -> String {
        let mut text = if state == State::Running {
            self.closing.clone()
        } else if self.word.is_some() {
            self.status_text.clone().unwrap_or_else(|| self.closing.clone())
        } else {
            self.closing.clone()
        };
        // no sentence and no closing words: what the app noted, whether the
        // run set its state itself or not
        if state != State::Running && text.is_empty() && matches!(state, State::Waiting | State::Blocked) {
            text = summary.needs_action.clone();
        }
        if state != State::Running && text.is_empty() {
            text = summary.detail.clone();
        }
        if text.chars().count() > 240 {
            text.chars().take(240).collect()
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::Kind;
    use crate::rules::{decide_state, Evidence};

    const NOW: f64 = 1_792_000_000_000.0;
    const H: f64 = 3_600_000.0;

    // a run that ended, through the same rules as every item
    fn run_state(rec: Option<&RunRecord>, summary: &Summary) -> (State, String) {
        let f = run_facts(rec, summary, None, None, NOW);
        let evidence = Evidence {
            word: f.word,
            has_record: rec.is_some(),
            record_open: f.record_open,
            refused: f.refused,
            quiet_shape: f.quiet_shape,
            ..Evidence::of(Kind::Recurring, summary.clone())
        };
        let state = decide_state(&evidence).0.gives;
        (state, f.reason(state, summary))
    }

    fn turn() -> Turn {
        Turn { by: "routine".into(), started_at: NOW - 2.0 * H, ended_at: Some(NOW - 2.0 * H + 30_000.0), tools: HashMap::from([("Bash".into(), 1.0)]), ..Default::default() }
    }

    fn record(turns: Vec<Turn>) -> RunRecord {
        RunRecord { session_id: "c".into(), task_id: "t".into(), started_at: NOW - 2.0 * H, updated_at: NOW - 2.0 * H + 60_000.0, turns, ..Default::default() }
    }

    fn blocked_summary(action: &str) -> Summary {
        Summary { category: Some("blocked".into()), needs_action: action.into(), ..Default::default() }
    }

    #[test]
    fn a_status_line_wins_without_one_the_app_summary_or_the_shape_decides() {
        let said = run_state(Some(&record(vec![Turn { status: Some("waiting".into()), status_text: Some("Bitte entscheiden.".into()), ..turn() }])), &Summary::default());
        assert_eq!(said.0, State::Waiting);
        assert_eq!(said.1, "Bitte entscheiden.");

        let quiet = run_state(Some(&record(vec![Turn { answer_tail: Some("Nichts Neues gefunden.".into()), ..turn() }])), &Summary::default());
        assert_eq!(quiet.0, State::Quiet);

        let filed = Summary { category: Some("review_ready".into()), detail: "no reviewed run yet".into(), ..Default::default() };
        assert_eq!(run_state(Some(&record(vec![turn()])), &filed).0, State::Quiet);

        // the app's "blocked" with nothing refused is a chat waiting on the person
        let waits = run_state(Some(&record(vec![turn()])), &blocked_summary("SSH erlauben"));
        assert_eq!(waits.0, State::Waiting);
        assert_eq!(waits.1, "SSH erlauben");

        let denied = Turn { permissions: vec![Permission { outcome: Some("denied".into()) }], ..turn() };
        let refused = run_state(Some(&record(vec![denied])), &blocked_summary("SSH erlauben"));
        assert_eq!(refused.0, State::Blocked);
    }

    #[test]
    fn a_refusal_is_settled_when_the_app_files_the_run_as_completed() {
        let denied = Turn { permissions: vec![Permission { outcome: Some("denied".into()) }], answer_tail: Some("x".repeat(250)), ..turn() };
        let settled = Summary { category: Some("completed".into()), detail: "settings.json valid".into(), ..Default::default() };
        assert_eq!(run_state(Some(&record(vec![denied.clone()])), &settled).0, State::Review);
    }

    #[test]
    fn the_closing_words_skip_a_code_block_and_keep_the_question() {
        let fenced = last_line(Some("Damit es klappt, braucht es eine Regel.\n\n```bash\nssh -L 18080:127.0.0.1:8080 host\n```"));
        assert_eq!(fenced, "Damit es klappt, braucht es eine Regel.");
        let long = "Der Stand ist unverändert und beide Punkte sind noch offen, weil bisher niemand etwas entschieden hat. ".repeat(3)
            + "Willst du ihn dauerhaft überspringen?";
        let asked = last_line(Some(&long));
        assert!(asked.ends_with("Willst du ihn dauerhaft überspringen?"));
        assert!(asked.starts_with("… "));
        let tail = last_line(Some("Der Lauf hat 0 Einträge.\n\nWillst du ihn dauerhaft überspringen, trag ihn in die State-Datei ein."));
        assert_eq!(tail, "Willst du ihn dauerhaft überspringen, trag ihn in die State-Datei ein.");
    }

    #[test]
    fn the_closing_words_skip_a_status_or_ping_line_in_any_form() {
        assert_eq!(last_line(Some("Fertig.\n\nSTATUS: idle")), "Fertig.");
        assert_eq!(last_line(Some("Konnte nicht.\n\n<!-- STATUS: blocked -->")), "Konnte nicht.");
        assert_eq!(last_line(Some("Erledigt.\n\n[//]: # (STATUS: blocked)")), "Erledigt.");
        assert_eq!(last_line(Some("Fertig.\n\n**STATUS: idle**")), "Fertig.");
        // the recorder reads the word in any case
        assert_eq!(last_line(Some("Konnte nicht.\n\n<!-- status: blocked -->")), "Konnte nicht.");
        assert_eq!(last_line(Some("Gesendet.\n\n<!-- PING: Push an mich (Handy) -->\n<!-- STATUS: worked -->")), "Gesendet.");
        assert_eq!(last_line(Some("Gesendet.\n\n**PING: Push an mich**\nSTATUS: worked")), "Gesendet.");
        // a state set without a sentence shows the closing words
        let blocked = Turn { status: Some("blocked".into()), answer_tail: Some("Konnte nicht.\n\n<!-- STATUS: blocked -->".into()), ..turn() };
        assert_eq!(run_state(Some(&record(vec![blocked])), &Summary::default()).1, "Konnte nicht.");
    }

    #[test]
    fn a_state_set_with_no_words_shows_what_the_app_noted() {
        let noted = Summary { detail: "Asked for SSH access".into(), ..blocked_summary("SSH erlauben") };
        let said = |status: &str, tail: &str| {
            let last = Turn { status: Some(status.into()), answer_tail: Some(tail.into()), ..turn() };
            run_state(Some(&record(vec![last])), &noted).1
        };
        assert_eq!(said("blocked", "STATUS: blocked"), "SSH erlauben");
        assert_eq!(said("waiting", "<!-- STATUS: waiting -->"), "SSH erlauben");
        // a run that says it is done asks nothing
        assert_eq!(said("worked", "STATUS: worked"), "Asked for SSH access");
        // Claude's own closing words still come first
        assert_eq!(said("blocked", "Konnte nicht.\n\nSTATUS: blocked"), "Konnte nicht.");
    }
}
