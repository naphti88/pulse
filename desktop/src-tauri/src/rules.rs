// The one place that decides what an item is. Readers (inputs.rs and those it
// calls) only gather facts into Evidence; here, the first rule of a list that
// holds wins, and a rule knows its name and what it means. That order is the
// whole logic: to know what wins over what, read the two lists from the top.
// Nothing else decides a state, and items.rs only draws the conclusions.
//
// Chats and routine runs go through the same lists. What told a state apart
// between them is a fact here (Evidence::kind), not a second tree.
//
// Which waits exist, and where each is told:
//
//   a choice or a plan put to the person   question_in_turn  (AskUserQuestion, ExitPlanMode)
//   a permission dialog on any tool        approval_pending  (the plugin sees the dialog)
//   an MCP server asks a question          approval_pending  (the plugin sees the request)
//   a shell or file call left open         approval_guess    (without the plugin: limits::PATIENCE_MS)
//
// Herdr's manifest for Claude (src/detect/manifests/claude.toml in its
// repository) was the checklist for these cases. Not told yet, because no
// transcript or hook shows them: a network approval while a command runs, and
// background MCP tasks.

use serde::Serialize;

use crate::items::{Kind, Stage, State};
use crate::runs::Summary;
use crate::store::MarkKind;

// every number the decisions lean on, in one place
pub mod limits {
    pub const HOUR_MS: f64 = 3_600_000.0;
    pub const DAY_MS: f64 = 24.0 * HOUR_MS;
    // how long a call may be open before it counts as waiting for approval; in
    // auto mode a classifier looks at a command before it runs, often 10 to 15
    // seconds, and the session goes on by itself whether it allows the command or not
    pub const PATIENCE_MS: f64 = 30_000.0;
    // a turn whose last entry is older than this is taken as ended (it crashed,
    // or the app was closed in the middle of it)
    pub const TURN_STALE_MS: f64 = 30.0 * 60_000.0;
    // a workflow started longer ago is taken as gone (the app was closed, the Mac slept)
    pub const WORKFLOW_MAX_AGE_MS: f64 = 12.0 * HOUR_MS;
    // a session or a record with no activity for so long has no turn open
    pub const LIVE_MS: f64 = 6.0 * HOUR_MS;
    // a session quiet for longer is no longer looked at for what runs
    pub const ACTIVE_MS: f64 = 7.0 * DAY_MS;
    // sessions older than this are left out, unless they still run
    pub const WINDOW_MS: f64 = 14.0 * DAY_MS;
    // activity this soon after parking still belongs to it (the app writes a title, a summary)
    pub const PARK_GRACE_MS: f64 = 60_000.0;
    // activity this soon after what was on screen when it was marked still belongs to it
    pub const MARK_GRACE_MS: f64 = 15_000.0;
    // a run that was this small and said nothing found nothing to do
    pub const QUIET_RUN_TOOLS: f64 = 8.0;
    pub const QUIET_RUN_CHARS: usize = 200;
    pub const QUIET_RUN_MS: f64 = 5.0 * 60_000.0;
}

// calls that put a question to the person: a choice to make, a plan to approve
pub const QUESTIONS: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];
// a shell command starts its process as soon as it may run; these tools are
// done in a moment
pub const INSTANT: [&str; 5] = ["Read", "Edit", "Write", "MultiEdit", "NotebookEdit"];
pub const SHELL: [&str; 2] = ["Bash", "PowerShell"];

// what a session says of its last answer: the STATUS line
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    Idle,
    Worked,
    Waiting,
    Blocked,
    Parked,
}

impl Word {
    pub const ALL: [(&'static str, Word); 5] =
        [("idle", Word::Idle), ("worked", Word::Worked), ("waiting", Word::Waiting), ("blocked", Word::Blocked), ("parked", Word::Parked)];

    pub fn parse(text: &str) -> Option<Word> {
        Word::ALL.iter().find(|(name, _)| *name == text).map(|(_, word)| *word)
    }

    pub fn as_str(self) -> &'static str {
        Word::ALL.iter().find(|(_, word)| *word == self).map_or("", |(name, _)| name)
    }
}

#[cfg(test)]
impl From<&str> for Word {
    fn from(text: &str) -> Word {
        Word::parse(text).expect("a status word")
    }
}

// (what it is, since when)
pub type Since = (String, f64);

// all that is known of a session at this moment, without a judgement
pub struct Evidence {
    pub kind: Kind,
    // a turn, a subagent or background work is open
    pub running: bool,
    // its transcript is in the middle of a turn
    pub in_turn: bool,
    // a routine's record has a turn open
    pub record_open: bool,
    // what it said: of a chat in its transcript, of a run in its record
    pub word: Option<Word>,
    // its last answer waits for the person, is blocked or parked
    pub needs_person: bool,
    pub summary: Summary,
    pub has_record: bool,
    // a permission was refused or left unanswered in the last round
    pub refused: bool,
    // a run that did so little that it found nothing to do
    pub quiet_shape: bool,
    pub asking: Option<Since>,
    pub pending: Option<Since>,
    pub awaiting: Option<Since>,
}

impl Evidence {
    pub fn of(kind: Kind, summary: Summary) -> Evidence {
        Evidence {
            kind,
            running: false,
            in_turn: false,
            record_open: false,
            word: None,
            needs_person: false,
            summary,
            has_record: false,
            refused: false,
            quiet_shape: false,
            asking: None,
            pending: None,
            awaiting: None,
        }
    }

    fn is_chat(&self) -> bool {
        self.kind == Kind::Chat
    }

    fn app_asks(&self) -> bool {
        !self.summary.needs_action.is_empty()
    }

    fn app_blocked(&self) -> bool {
        self.summary.category.as_deref() == Some("blocked")
    }
}

pub struct Rule<T: 'static, E: 'static> {
    pub id: &'static str,
    pub gives: T,
    pub says: &'static str,
    when: fn(&E) -> bool,
}

type StateRule = Rule<State, Evidence>;

macro_rules! rule {
    ($id:literal, $gives:expr, $says:literal, $when:expr) => {
        Rule { id: $id, gives: $gives, says: $says, when: $when }
    };
}

// from the top, the first that holds decides
pub static STATE_RULES: [StateRule; 15] = [
    // a call that waits for the person holds the session up, whatever else is true
    rule!("question_in_turn", State::Waiting, "It put a choice or a plan to you in the middle of its turn.", |e| e.asking.is_some()),
    rule!("approval_pending", State::Blocked, "The plugin saw a request for your approval that was not answered yet.", |e| e.pending.is_some()),
    rule!("approval_guess", State::Blocked, "A call stayed open too long without its process: it must wait for your approval.", |e| e.awaiting.is_some()),
    // a turn goes on, whatever was said before
    rule!("turn_open", State::Running, "Its transcript is in the middle of a turn.", |e| e.running && e.in_turn),
    // the turn ended; an answer that needs the person comes before work in the background
    rule!("background_work", State::Running, "Its turn ended, and work in the background goes on.", |e| e.running && !e.needs_person),
    rule!("record_open", State::Running, "The routine's record has a turn open.", |e| !e.is_chat() && e.record_open),
    // what the session said of its last answer
    rule!("said_waiting", State::Waiting, "Its last answer said it waits for you.", |e| e.word == Some(Word::Waiting)),
    rule!("said_blocked", State::Blocked, "Its last answer said it could not go on.", |e| e.word == Some(Word::Blocked)),
    rule!("said_idle", State::Quiet, "Its last answer said there was nothing to do.", |e| e.word == Some(Word::Idle)),
    rule!("said_result", State::Review, "Its last answer said the work is done.", |e| matches!(e.word, Some(Word::Worked | Word::Parked))),
    // no STATUS line: what the app filed it under. The app calls a chat that
    // waits on the person blocked, so only a refused permission blocks a run
    rule!("app_refused", State::Blocked, "The app says it is blocked, and a permission was refused.", |e| {
        !e.is_chat() && e.app_asks() && e.app_blocked() && e.refused
    }),
    rule!("app_asks", State::Waiting, "The app's summary says it asks you something.", |e| e.app_asks() || (e.is_chat() && e.app_blocked())),
    // no STATUS line and nothing filed: a run is judged by its shape
    rule!("nothing_known", State::Review, "Nothing is known of this run.", |e| !e.is_chat() && !e.has_record && e.summary.category.is_none()),
    rule!("run_found_nothing", State::Quiet, "The run was too small to have found anything.", |e| !e.is_chat() && e.quiet_shape),
    rule!("result", State::Review, "It ended, and nothing says it needs you.", |_| true),
];

pub struct StageEvidence {
    pub state: State,
    pub is_parked: bool,
    // the person's mark, if it still covers what is there
    pub mark: Option<MarkKind>,
    // the app's unread dot
    pub unread: bool,
    // unparked it is back on the table, read long ago or not
    pub resumed: bool,
    // an answer nobody was there for has no reader yet (items::sync_unseen)
    pub unseen: bool,
}

type StageRule = Rule<Stage, StageEvidence>;

pub static STAGE_RULES: [StageRule; 8] = [
    rule!("parked", Stage::Parked, "Set aside until it goes on.", |e| e.is_parked),
    rule!("running", Stage::Running, "It runs.", |e| e.state == State::Running),
    rule!("found_nothing", Stage::Done, "It found nothing to do.", |e| e.state == State::Quiet),
    rule!("marked_done", Stage::Done, "You closed it.", |e| e.mark == Some(MarkKind::Done)),
    // a result is only to be read: once opened in Claude it is done
    rule!("result_seen", Stage::Done, "A result you marked as read.", |e| e.state == State::Review && e.mark == Some(MarkKind::Seen)),
    rule!("result_read", Stage::Done, "A result the app has no unread dot for.", |e| {
        e.state == State::Review && e.mark.is_none() && !e.unread && !e.resumed && !e.unseen
    }),
    // a question, open points or a block stay open, read or not, until they
    // are marked done or the session goes on; "later" sets them aside
    rule!("later", Stage::Follow, "You set it aside: read, still open.", |e| e.mark == Some(MarkKind::Seen)),
    rule!("open", Stage::Open, "It waits for you.", |_| true),
];

// the rule that decides, and the names of those that came before it and did not hold
fn first<T, E>(rules: &'static [Rule<T, E>], evidence: &E) -> (&'static Rule<T, E>, Vec<&'static str>) {
    let at = rules.iter().position(|rule| (rule.when)(evidence)).unwrap_or(rules.len() - 1);
    (&rules[at], rules[..at].iter().map(|rule| rule.id).collect())
}

pub fn decide_state(evidence: &Evidence) -> (&'static StateRule, Vec<&'static str>) {
    first(&STATE_RULES, evidence)
}

pub fn decide_stage(evidence: &StageEvidence) -> (&'static StageRule, Vec<&'static str>) {
    first(&STAGE_RULES, evidence)
}

// why an item is what it is, for a person to read
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Why {
    pub rule: String,
    pub says: String,
    pub stage_rule: String,
    pub stage_says: String,
    // the rules before it, that did not hold
    pub passed: Vec<String>,
    // what was known, one fact a line
    pub facts: Vec<String>,
}

fn yes(on: bool) -> &'static str {
    if on {
        "yes"
    } else {
        "no"
    }
}

impl Evidence {
    // the facts in words; a rule's text says the rest
    pub fn facts(&self, now: f64) -> Vec<String> {
        let waits = |name: &str, what: &Option<Since>| what.as_ref().map(|(what, at)| format!("{name}: {what}, for {} s", ((now - at) / 1000.0).max(0.0) as i64));
        let mut facts = vec![
            format!("Turn open: {}", yes(self.in_turn)),
            format!("Running in any way: {}", yes(self.running)),
            format!("Said: {}", self.word.map_or("nothing", Word::as_str)),
        ];
        facts.extend(waits("Question", &self.asking));
        facts.extend(waits("Approval seen by the plugin", &self.pending));
        facts.extend(waits("Call open", &self.awaiting));
        if self.summary.category.is_some() || self.app_asks() {
            facts.push(format!("App: {}, asks you: {}", self.summary.category.as_deref().unwrap_or("not filed"), yes(self.app_asks())));
        }
        if !self.is_chat() {
            facts.push(format!("Record: {}, turn open: {}, refused: {}", yes(self.has_record), yes(self.record_open), yes(self.refused)));
        }
        facts
    }
}

impl StageEvidence {
    pub fn facts(&self) -> Vec<String> {
        let mark = match self.mark {
            Some(MarkKind::Done) => "done",
            Some(MarkKind::Seen) => "later",
            None => "none",
        };
        vec![format!("Your mark: {mark}"), format!("Unread: {}", yes(self.unread || self.unseen)), format!("Parked: {}", yes(self.is_parked))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_rule_has_a_name_of_its_own_and_the_last_always_holds() {
        let names: Vec<&str> = STATE_RULES.iter().map(|r| r.id).chain(STAGE_RULES.iter().map(|r| r.id)).collect();
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
        assert_eq!(decide_state(&Evidence::of(Kind::Chat, Summary::default())).0.id, "result");
        let stage = StageEvidence { state: State::Waiting, is_parked: false, mark: None, unread: false, resumed: false, unseen: false };
        assert_eq!(decide_stage(&stage).0.id, "open");
    }

    #[test]
    fn a_wait_comes_before_anything_else_and_the_rules_that_passed_are_named() {
        let call = ("Bash · make test".to_string(), 0.0);
        let mut e = Evidence { running: true, in_turn: true, ..Evidence::of(Kind::Chat, Summary::default()) };
        assert_eq!(decide_state(&e).0.gives, State::Running);
        e.awaiting = Some(call.clone());
        assert_eq!(decide_state(&e).0.id, "approval_guess");
        e.pending = Some(call.clone());
        assert_eq!(decide_state(&e).0.id, "approval_pending");
        e.asking = Some(call);
        let (rule, passed) = decide_state(&e);
        assert_eq!((rule.id, passed.len()), ("question_in_turn", 0));
        e.asking = None;
        assert_eq!(decide_state(&e).1, vec!["question_in_turn"]);
    }

    #[test]
    fn the_words_of_a_status_line() {
        for (name, word) in Word::ALL {
            assert_eq!((Word::parse(name), word.as_str()), (Some(word), name));
        }
        assert_eq!(Word::parse("running"), None);
    }
}
