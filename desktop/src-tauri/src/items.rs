// From what was read to what Pulse shows: every session, chat or routine run
// alike, is one item with a state (what it is doing or needs) and a stage
// (where it is in its life: running, open, follow up, done). Next to the items
// the runs that are planned and the slots that were missed.

use std::collections::HashMap;

use serde::Serialize;

use crate::checks::Check;
use crate::cron::{self, DAY, HOUR, MINUTE};
use crate::inputs::{Inputs, Session, TaskDef};
use crate::live::Live;
use crate::rules::limits::{MARK_GRACE_MS, PARK_GRACE_MS, WINDOW_MS};
use crate::rules::{decide_stage, decide_state, Evidence, StageEvidence, Why, Word};
use crate::runs::{run_facts, RunRecord};
use crate::store::{Parked, Store};
use crate::subagents::SubagentView;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Blocked,
    Waiting,
    Review,
    // ran and found nothing to do
    Quiet,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Running,
    Open,
    Follow,
    // set aside to come back to later
    Parked,
    Done,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Recurring,
    Oneoff,
    Chat,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub title: String,
    pub project: String,
    pub kind: Kind,
    pub state: State,
    pub stage: Stage,
    // since when it is in its state: the end of its last activity
    pub since: f64,
    pub started_at: f64,
    pub reason: String,
    pub parent_id: Option<String>,
    pub live: Option<Live>,
    pub subagents: Vec<SubagentView>,
    // when the app last brought it on show
    #[serde(skip)]
    pub focused: f64,
    // what it changed that has not landed, in words
    pub work: Vec<String>,
    // which rules made it what it is
    pub why: Why,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Planned {
    pub title: String,
    pub project: String,
    pub kind: Kind,
    pub at: f64,
    // a routine that runs more than once in the window: how often, and the last time
    pub count: usize,
    pub last: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Missed {
    pub title: String,
    pub project: String,
    pub at: f64,
}

#[derive(Serialize, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub parked: usize,
    pub blocked: usize,
    pub waiting: usize,
    pub review: usize,
    pub running: usize,
    pub follow: usize,
    // commands worth a look, not marked as known (src/checks.rs)
    pub commands: usize,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub now: f64,
    pub tz_offset_min: i64,
    pub items: Vec<Item>,
    pub planned: Vec<Planned>,
    pub missed: Vec<Missed>,
    pub counts: Counts,
    // the commands the sessions run now, those worth a look first
    pub checks: Vec<Check>,
    // the topics set aside, the latest first
    pub parked: Vec<ParkedView>,
    pub problems: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ParkedView {
    pub id: String,
    pub title: String,
    pub project: String,
    // where it stands and what comes next
    pub said: String,
    pub at: f64,
    // the app no longer keeps the session: a note only
    pub gone: bool,
}

// the project a folder belongs to: a worktree counts to its repository
pub fn project_of(path: &str) -> String {
    let root = path.split("/.claude/worktrees/").next().unwrap_or(path).trim_end_matches('/');
    root.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(root).to_string()
}

// "Acme (Data) - Preis-Watch (werktags 11:30)" → "Preis-Watch":
// the schedule in brackets at the end and a prefix before " - " are left out
pub fn clean_name(name: &str) -> String {
    let mut text = name.trim();
    if text.ends_with(')') {
        if let Some(open) = text.rfind(" (") {
            text = text[..open].trim_end();
        }
    }
    match text.split_once(" - ") {
        Some((_, rest)) if !rest.trim().is_empty() => rest.trim().to_string(),
        _ => text.to_string(),
    }
}

pub fn kind_of(task: Option<&TaskDef>) -> Kind {
    match task {
        Some(t) if t.cron.is_none() && t.fire_at.is_some() => Kind::Oneoff,
        _ => Kind::Recurring,
    }
}

fn item_of(s: &Session, input: &Inputs, records: &HashMap<&str, &RunRecord>, tasks: &HashMap<&str, &TaskDef>, store: &Store) -> Item {
    let now = input.now;
    let rec = s.cli_id.as_deref().and_then(|id| records.get(id).copied());
    let said = input.said.get(&s.id);
    let recorded = rec.and_then(|r| r.turns.last()).and_then(|t| t.status.as_deref()).and_then(Word::parse);
    // what its last answer said: of a chat from its transcript, of a run from its record
    let last_said = said.map(|said| said.state).or(recorded);
    let task_id = s.task_id.as_deref().or(rec.map(|r| r.task_id.as_str()));
    let task = task_id.and_then(|id| tasks.get(id).copied());
    let kind = if task_id.is_some() { kind_of(task) } else { Kind::Chat };
    let run = (kind != Kind::Chat).then(|| run_facts(rec, &s.summary, s.created_at, s.last_activity, now));

    let evidence = Evidence {
        running: input.running.contains(&s.id),
        in_turn: input.in_turn.contains(&s.id),
        record_open: run.as_ref().is_some_and(|r| r.record_open),
        word: if kind == Kind::Chat { said.map(|said| said.state) } else { run.as_ref().and_then(|r| r.word) },
        needs_person: matches!(last_said, Some(Word::Waiting | Word::Blocked | Word::Parked)),
        has_record: rec.is_some(),
        refused: run.as_ref().is_some_and(|r| r.refused),
        quiet_shape: run.as_ref().is_some_and(|r| r.quiet_shape),
        asking: input.asking.get(&s.id).cloned(),
        engine: input.engines.by_id.get(&s.id).map(|e| e.status.clone()),
        engine_approval: input.engine_approval.get(&s.id).cloned(),
        engine_question: input.engine_question.get(&s.id).cloned(),
        pending: input.pending.get(&s.id).cloned(),
        awaiting: input.awaiting.get(&s.id).cloned(),
        failed: input.failed.get(&s.id).cloned(),
        ..Evidence::of(kind, s.summary.clone())
    };
    let (rule, passed) = decide_state(&evidence);
    let state = rule.gives;

    let activity = s.last_activity.unwrap_or(0.0);
    let (started_at, last_activity, words) = match &run {
        Some(run) => (run.started_at, run.last_activity, run.reason(state, &s.summary)),
        None => (s.created_at.unwrap_or(activity), activity, s.summary.words(said.map(|said| said.text.clone()))),
    };
    let approval = evidence.engine_approval.as_ref().or(evidence.pending.as_ref()).or(evidence.awaiting.as_ref());
    let (reason, since) = match (&evidence.asking, &evidence.engine_question, approval) {
        (Some((question, at)), _, _) => (question.clone(), *at),
        (None, _, Some((what, at))) if rule.gives == State::Blocked => (format!("Waiting for approval: {what}"), *at),
        (None, Some((what, at)), _) => (format!("Waiting for your answer: {what}"), *at),
        _ => match evidence.failed.as_ref().filter(|_| rule.id == "turn_failed") {
            Some((what, at)) => (format!("Stopped: {what}"), *at),
            None => (words, last_activity),
        },
    };

    // parked by its last answer, unless taken up again in the panel since; or
    // parked in the panel, until the session goes on
    let said_parked = said.is_some_and(|said| said.state == Word::Parked) || (kind != Kind::Chat && recorded == Some(Word::Parked));
    let resumed = store.resumed.get(&s.id).is_some_and(|&at| at >= since);
    let stage_evidence = StageEvidence {
        state,
        is_parked: state != State::Running
            && ((said_parked && !resumed) || store.parked.get(&s.id).is_some_and(|p| since <= p.at + PARK_GRACE_MS)),
        mark: store.marks.get(&s.id).filter(|m| m.at >= since && m.shown.is_none_or(|shown| since <= shown + MARK_GRACE_MS)).map(|m| m.kind),
        unread: input.unread.contains(&s.id),
        resumed,
        unseen: store.unseen.contains_key(&s.id),
        work_left: input.work.contains_key(&s.id),
    };
    let (stage_rule, _) = decide_stage(&stage_evidence);

    let title = match (task, &s.title) {
        (Some(t), _) => clean_name(&t.name),
        (None, Some(title)) => title.clone(),
        (None, None) => task_id.map(clean_name).unwrap_or_else(|| s.id.clone()),
    };
    let folder = s.origin_cwd.as_deref().or(s.cwd.as_deref()).or(task.and_then(|t| t.cwd.as_deref())).unwrap_or("");
    let work = input.work.get(&s.id).cloned().unwrap_or_default();
    let mut facts = evidence.facts(now);
    facts.extend(stage_evidence.facts());
    facts.extend(work.iter().map(|note| format!("Not landed: {note}")));
    Item {
        id: s.id.clone(),
        title,
        project: project_of(folder),
        kind,
        state,
        stage: stage_rule.gives,
        since,
        started_at,
        reason,
        parent_id: s.parent_id.clone(),
        live: input.live.get(&s.id).cloned(),
        subagents: input.subagents.get(&s.id).cloned().unwrap_or_default(),
        focused: s.last_focused.unwrap_or(0.0),
        work,
        why: Why {
            rule: rule.id.to_string(),
            says: rule.says.to_string(),
            stage_rule: stage_rule.id.to_string(),
            stage_says: stage_rule.says.to_string(),
            passed: passed.iter().map(ToString::to_string).collect(),
            facts,
        },
    }
}

// the slots of a routine that passed without a run, in the last two days; a
// slot younger than 15 minutes may still be in its jitter
fn missed_slots(cron: &cron::Cron, task: &TaskDef, starts: &[i64], skips: &[f64], now: i64, tz: i64) -> Vec<i64> {
    let from = (now - 2 * DAY).max(task.created_at.unwrap_or(0.0) as i64);
    let slots = cron.slots_between(from, now, tz);
    let mut used = vec![false; starts.len()];
    let mut missed = Vec::new();
    for (i, &slot) in slots.iter().enumerate() {
        let end = slots.get(i + 1).copied().or_else(|| cron.next_slot(slot, tz)).unwrap_or(now);
        let run = (0..starts.len()).find(|&k| !used[k] && starts[k] >= slot - 2 * MINUTE && starts[k] < end);
        if let Some(k) = run {
            used[k] = true;
            continue;
        }
        let is_skipped = skips.iter().any(|&s| s as i64 >= slot - 2 * MINUTE && (s as i64) < end);
        if slot <= now - 15 * MINUTE && !is_skipped {
            missed.push(slot);
        }
    }
    missed
}

pub fn build(input: &Inputs, store: &Store, tz: i64) -> Snapshot {
    let now = input.now;
    let records: HashMap<&str, &RunRecord> = input.records.iter().map(|r| (r.session_id.as_str(), r.as_ref())).collect();
    let tasks: HashMap<&str, &TaskDef> = input.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    let mut items: Vec<Item> = input
        .sessions
        .iter()
        // a parked topic stays, however old, archived or not
        .filter(|s| input.running.contains(&s.id) || store.parked.contains_key(&s.id) || (!s.archived && s.last_activity.unwrap_or(0.0) > now - WINDOW_MS))
        .map(|s| item_of(s, input, &records, &tasks, store))
        .collect();
    items.sort_by(|a, b| b.since.total_cmp(&a.since));

    let mut planned = Vec::new();
    let mut missed = Vec::new();
    let now_i = now as i64;
    for task in input.tasks.iter().filter(|t| t.enabled) {
        let project = project_of(task.cwd.as_deref().unwrap_or(""));
        let title = clean_name(&task.name);
        if let Some(c) = task.cron.as_deref().and_then(cron::parse) {
            let slots = c.slots_between(now_i + MINUTE, now_i + 36 * HOUR, tz);
            if let (Some(&first), Some(&last)) = (slots.first(), slots.last()) {
                planned.push(Planned { title: title.clone(), project: project.clone(), kind: Kind::Recurring, at: first as f64, count: slots.len(), last: last as f64 });
            }
            let mut starts: Vec<i64> = input.records.iter().filter(|r| r.task_id == task.id).map(|r| r.started_at as i64).collect();
            starts.extend(
                input
                    .sessions
                    .iter()
                    .filter(|s| s.task_id.as_deref() == Some(task.id.as_str()) && !s.cli_id.as_deref().is_some_and(|id| records.contains_key(id)))
                    .filter_map(|s| s.created_at.map(|t| t as i64)),
            );
            starts.sort_unstable();
            let skips = input.skips.get(&task.id).map(Vec::as_slice).unwrap_or(&[]);
            for at in missed_slots(&c, task, &starts, skips, now_i, tz) {
                missed.push(Missed { title: title.clone(), project: project.clone(), at: at as f64 });
            }
        } else if let Some(at) = task.fire_at.filter(|&at| at > now && at < now + 7.0 * DAY as f64) {
            planned.push(Planned { title, project, kind: Kind::Oneoff, at, count: 1, last: at });
        }
    }
    planned.sort_by(|a, b| a.at.total_cmp(&b.at));
    missed.sort_by(|a, b| b.at.total_cmp(&a.at));

    let open = |state: State| items.iter().filter(|i| i.stage == Stage::Open && i.state == state).count();
    let counts = Counts {
        blocked: open(State::Blocked),
        waiting: open(State::Waiting),
        review: open(State::Review),
        running: items.iter().filter(|i| i.stage == Stage::Running).count(),
        follow: items.iter().filter(|i| i.stage == Stage::Follow).count(),
        commands: 0,
        parked: 0,
    };
    let parked = parked_of(&items, input, store);
    let counts = Counts { parked: parked.len(), ..counts };
    Snapshot { now, tz_offset_min: tz, items, planned, missed, counts, checks: Vec::new(), parked, problems: input.problems.clone() }
}

// the parked items, and the notes of parked topics whose session the app no longer keeps
fn parked_of(items: &[Item], input: &Inputs, store: &Store) -> Vec<ParkedView> {
    let mut list: Vec<ParkedView> = items
        .iter()
        .filter(|i| i.stage == Stage::Parked)
        .map(|i| {
            let note = store.parked.get(&i.id);
            let said = if i.reason.is_empty() { note.map(|n| n.said.clone()).unwrap_or_default() } else { i.reason.clone() };
            ParkedView { id: i.id.clone(), title: i.title.clone(), project: i.project.clone(), said, at: note.map_or(i.since, |n| n.at), gone: false }
        })
        .collect();
    for (id, note) in &store.parked {
        if !input.sessions.iter().any(|s| &s.id == id) {
            list.push(ParkedView { id: id.clone(), title: note.title.clone(), project: note.project.clone(), said: note.said.clone(), at: note.at, gone: true });
        }
    }
    list.sort_by(|a, b| b.at.total_cmp(&a.at));
    list
}

// keep the notes in step with the items: a topic parked by its answer gets
// its copy, one that went on loses it, one taken up again forgets that once
// it went on; returns whether the store changed
pub fn sync_parked(snap: &Snapshot, store: &mut Store) -> bool {
    let mut changed = false;
    for view in snap.parked.iter().filter(|v| !v.gone) {
        let note = Parked { at: view.at, title: view.title.clone(), project: view.project.clone(), said: view.said.clone() };
        if store.parked.get(&view.id) != Some(&note) {
            store.parked.insert(view.id.clone(), note);
            changed = true;
        }
    }
    let before = (store.parked.len(), store.resumed.len());
    store.parked.retain(|id, _| snap.parked.iter().any(|v| &v.id == id));
    store.resumed.retain(|id, at| snap.items.iter().any(|i| &i.id == id && i.since <= *at));
    changed || before != (store.parked.len(), store.resumed.len())
}

// the app gives no unread dot to an answer in the session it has on show,
// even with the person gone: a result that ends with Claude not in front
// counts as unread until the session is opened or goes on; returns whether the
// store changed
pub fn sync_unseen(items: &[Item], before: &[Item], store: &mut Store, in_front: bool) -> bool {
    let count = store.unseen.len();
    store.unseen.retain(|id, at| items.iter().any(|i| &i.id == id && i.stage != Stage::Running && i.focused < *at));
    let mut changed = count != store.unseen.len();
    if !in_front {
        let ended = |i: &Item| before.iter().any(|b| b.id == i.id && b.stage == Stage::Running);
        for item in items.iter().filter(|i| i.state == State::Review && i.focused < i.since && ended(i)) {
            store.unseen.insert(item.id.clone(), item.since);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::{Summary, Turn};
    use crate::said::Said;
    use crate::store::{Mark, MarkKind};
    use std::collections::HashSet;
    use std::sync::Arc;

    const NOW: f64 = 1_792_000_000_000.0;
    const H: f64 = HOUR as f64;

    fn inputs(sessions: Vec<Session>, records: Vec<RunRecord>, tasks: Vec<TaskDef>) -> Inputs {
        Inputs { tasks, sessions, records: records.into_iter().map(Arc::new).collect(), ..Inputs::empty(NOW) }
    }

    fn session(id: &str, activity: f64) -> Session {
        Session {
            id: id.into(),
            cli_id: Some(format!("cli-{id}")),
            title: Some(format!("Chat {id}")),
            cwd: Some("/Users/x/Work/shop-api/.claude/worktrees/bold-x".into()),
            folders: Vec::new(),
            origin_cwd: None,
            archived: false,
            created_at: Some(activity - H),
            last_activity: Some(activity),
            last_focused: None,
            task_id: None,
            parent_id: None,
            written: Vec::new(),
            prs: Vec::new(),
            summary: Summary::default(),
        }
    }

    fn task(id: &str, cron: &str) -> TaskDef {
        TaskDef { id: id.into(), name: format!("Acme - {id} (täglich)"), cron: Some(cron.into()), fire_at: None, enabled: true, created_at: Some(NOW - 5.0 * DAY as f64), cwd: Some("/Users/x/Work/shop-api".into()) }
    }

    fn run(id: &str, task: &str, status: &str) -> RunRecord {
        RunRecord {
            session_id: format!("cli-{id}"),
            task_id: task.into(),
            started_at: NOW - 2.0 * H,
            updated_at: NOW - 2.0 * H + 60_000.0,
            turns: vec![Turn { by: "routine".into(), started_at: NOW - 2.0 * H, status: Some(status.into()), status_text: Some("Text".into()), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn names_and_projects() {
        assert_eq!(clean_name("Acme (Data) - Preis-Watch (werktags 11:30)"), "Preis-Watch");
        assert_eq!(clean_name("Newsletter-Digest (täglich 08:30)"), "Newsletter-Digest");
        assert_eq!(project_of("/Users/x/Work/shop-api/.claude/worktrees/bold-x"), "shop-api");
        assert_eq!(project_of("/Users/x/Work/shop-web"), "shop-web");
    }

    #[test]
    fn a_run_that_worked_is_a_result_and_an_idle_one_is_done() {
        let mut worked = session("a", NOW - H);
        worked.task_id = Some("daily".into());
        let mut idle = session("b", NOW - H);
        idle.task_id = Some("daily".into());
        let mut input = inputs(vec![worked, idle], vec![run("a", "daily", "worked"), run("b", "daily", "idle")], vec![task("daily", "0 7 * * *")]);
        input.unread = HashSet::from(["a".to_string(), "b".to_string()]);
        let snap = build(&input, &Store::default(), 60);
        let a = snap.items.iter().find(|i| i.id == "a").unwrap();
        assert_eq!((a.state, a.stage, a.kind), (State::Review, Stage::Open, Kind::Recurring));
        assert_eq!(a.title, "daily");
        let b = snap.items.iter().find(|i| i.id == "b").unwrap();
        assert_eq!(b.stage, Stage::Done);
        assert_eq!(snap.counts.review, 1);
    }

    #[test]
    fn a_mark_holds_until_the_item_changes() {
        let mut ses = session("q", NOW - H);
        ses.summary = Summary { category: Some("blocked".into()), needs_action: "Soll ich das einbauen?".into(), ..Default::default() };
        let input = inputs(vec![ses.clone()], vec![], vec![]);
        let open = build(&input, &Store::default(), 60);
        assert_eq!((open.items[0].state, open.items[0].stage), (State::Waiting, Stage::Open));
        assert_eq!(open.items[0].project, "shop-api");

        let mut store = Store::default();
        store.marks.insert("q".into(), Mark { kind: MarkKind::Seen, at: NOW, shown: None });
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Follow);
        store.marks.insert("q".into(), Mark { kind: MarkKind::Done, at: NOW, shown: None });
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Done);

        // it answered again after the mark: open again
        ses.last_activity = Some(NOW + H);
        let mut later = inputs(vec![ses], vec![], vec![]);
        later.now = NOW + 2.0 * H;
        assert_eq!(build(&later, &store, 60).items[0].stage, Stage::Open);
    }

    #[test]
    fn a_result_opened_in_claude_is_done_and_an_archived_session_is_gone() {
        let read = session("r", NOW - H);
        let mut archived = session("s", NOW - H);
        archived.archived = true;
        let snap = build(&inputs(vec![read, archived], vec![], vec![]), &Store::default(), 60);
        assert_eq!(snap.items.len(), 1);
        assert_eq!((snap.items[0].state, snap.items[0].stage), (State::Review, Stage::Done));
    }

    #[test]
    fn parked_by_its_answer_or_in_the_panel_until_it_goes_on() {
        let note = |at: f64| Parked { at, title: "RAM".into(), project: "shop-api".into(), said: "Messung steht".into() };
        // parked by its last answer
        let mut input = inputs(vec![session("a", NOW - H)], vec![], vec![]);
        input.said.insert("a".into(), Said { state: "parked".into(), text: "Nächster Schritt: Profil".into() });
        let mut store = Store::default();
        let snap = build(&input, &store, 60);
        assert_eq!((snap.items[0].stage, snap.counts.parked), (Stage::Parked, 1));
        assert_eq!(snap.parked[0].said, "Nächster Schritt: Profil");
        assert!(sync_parked(&snap, &mut store));
        assert_eq!(store.parked.get("a").map(|p| p.at), Some(NOW - H));
        // taken up again in the panel (as the command does): open, the note gone
        store.parked.remove("a");
        store.resumed.insert("a".into(), NOW);
        let snap = build(&input, &store, 60);
        assert_eq!(snap.items[0].stage, Stage::Open);
        sync_parked(&snap, &mut store);
        assert!(store.parked.is_empty());

        // parked in the panel, archived and old: still there, until it goes on
        let mut old = session("b", NOW - 30.0 * DAY as f64);
        old.archived = true;
        let mut input = inputs(vec![old.clone()], vec![], vec![]);
        let mut store = Store::default();
        store.parked.insert("b".into(), note(NOW - H));
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Parked);
        // looked at in Claude: the time it was opened changes nothing
        old.last_focused = Some(NOW);
        input.sessions = vec![old.clone()];
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Parked);
        old.last_activity = Some(NOW);
        input.sessions = vec![old];
        let snap = build(&input, &store, 60);
        assert_ne!(snap.items[0].stage, Stage::Parked);
        sync_parked(&snap, &mut store);
        assert!(store.parked.is_empty());

        // the app no longer keeps the session: the note stays
        let input = inputs(vec![], vec![], vec![]);
        let mut store = Store::default();
        store.parked.insert("gone".into(), note(NOW - H));
        let snap = build(&input, &store, 60);
        assert_eq!((snap.parked.len(), snap.parked[0].gone), (1, true));
        assert!(!sync_parked(&snap, &mut store));
        assert_eq!(store.parked.len(), 1);
    }

    #[test]
    fn a_mark_covers_what_was_on_screen() {
        let mut input = inputs(vec![session("a", NOW - 1000.0)], vec![], vec![]);
        input.said.insert("a".into(), Said { state: "worked".into(), text: "Fertig".into() });
        input.unread.insert("a".into());
        let mut store = Store::default();
        // clicked on the question while the next answer came in
        store.marks.insert("a".into(), Mark { kind: MarkKind::Done, at: NOW, shown: Some(NOW - 100_000.0) });
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Open);
        // clicked on that answer
        store.marks.insert("a".into(), Mark { kind: MarkKind::Done, at: NOW, shown: Some(NOW - 1000.0) });
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Done);
    }

    #[test]
    fn an_answer_with_nobody_at_the_app_is_unread_without_a_dot() {
        let mut quiet = session("a", NOW - 1000.0);
        quiet.last_focused = Some(NOW - 20_000.0);
        let mut input = inputs(vec![quiet.clone()], vec![], vec![]);
        input.said.insert("a".into(), Said { state: "worked".into(), text: "Fertig".into() });
        let mut store = Store::default();
        let mut running = build(&input, &store, 60).items;
        assert_eq!(running[0].stage, Stage::Done);
        running[0].stage = Stage::Running;
        let items = build(&input, &store, 60).items;

        // Claude in front: read as it came
        assert!(!sync_unseen(&items, &running, &mut store, true));
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Done);

        // not in front: open, also on the next look
        assert!(sync_unseen(&items, &running, &mut store, false));
        let items = build(&input, &store, 60).items;
        assert_eq!(items[0].stage, Stage::Open);
        assert!(!sync_unseen(&items, &items, &mut store, false));
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Open);

        // the session is brought on show: read
        quiet.last_focused = Some(NOW);
        input.sessions = vec![quiet];
        let items = build(&input, &store, 60).items;
        assert!(sync_unseen(&items, &items, &mut store, false));
        assert!(store.unseen.is_empty());
        assert_eq!(build(&input, &store, 60).items[0].stage, Stage::Done);
    }

    #[test]
    fn an_answer_that_needs_the_person_comes_before_work_in_the_background() {
        let mut input = inputs(vec![session("a", NOW - H)], vec![], vec![]);
        input.running.insert("a".into());
        input.said.insert("a".into(), Said { state: "worked".into(), text: "Tunnel steht".into() });
        assert_eq!(build(&input, &Store::default(), 60).items[0].stage, Stage::Running);
        input.said.insert("a".into(), Said { state: "waiting".into(), text: "Item 545 freigeben".into() });
        input.unread.insert("a".into());
        let item = build(&input, &Store::default(), 60).items.remove(0);
        assert_eq!((item.state, item.stage, item.reason.as_str()), (State::Waiting, Stage::Open, "Item 545 freigeben"));
        // in the middle of a turn it runs, whatever it said before
        input.in_turn.insert("a".into());
        assert_eq!(build(&input, &Store::default(), 60).items[0].stage, Stage::Running);
    }

    #[test]
    fn a_running_session_that_waits_for_approval_is_blocked() {
        let mut input = inputs(vec![session("a", NOW)], vec![], vec![]);
        input.running.insert("a".into());
        assert_eq!(build(&input, &Store::default(), 60).items[0].stage, Stage::Running);
        input.awaiting.insert("a".into(), ("Bash · make test".into(), NOW - 10_000.0));
        let item = build(&input, &Store::default(), 60).items.remove(0);
        assert_eq!((item.state, item.stage, item.reason.as_str()), (State::Blocked, Stage::Open, "Waiting for approval: Bash · make test"));
    }

    #[test]
    fn open_points_stay_open_after_reading_a_result_does_not() {
        let said = |state: &str| Said { state: state.into(), text: format!("{state} gesagt") };
        let mut input = inputs(vec![session("w", NOW - H), session("r", NOW - H), session("b", NOW - H)], vec![], vec![]);
        input.said = HashMap::from([("w".into(), said("waiting")), ("r".into(), said("worked")), ("b".into(), said("blocked"))]);
        // all of them read in Claude
        let snap = build(&input, &Store::default(), 60);
        let of = |id: &str| snap.items.iter().find(|i| i.id == id).map(|i| (i.state, i.stage, i.reason.clone()));
        assert_eq!(of("w"), Some((State::Waiting, Stage::Open, "waiting gesagt".into())));
        assert_eq!(of("r"), Some((State::Review, Stage::Done, "worked gesagt".into())));
        assert_eq!(of("b").map(|o| (o.0, o.1)), Some((State::Blocked, Stage::Open)));

        // unread, a result is open; "later" sets open points aside, a result it closes
        input.unread = HashSet::from(["r".to_string()]);
        let mut store = Store::default();
        assert_eq!(build(&input, &store, 60).items.iter().find(|i| i.id == "r").map(|i| i.stage), Some(Stage::Open));
        store.marks.insert("w".into(), Mark { kind: MarkKind::Seen, at: NOW, shown: None });
        store.marks.insert("r".into(), Mark { kind: MarkKind::Seen, at: NOW, shown: None });
        let later = build(&input, &store, 60);
        let stage = |id: &str| later.items.iter().find(|i| i.id == id).map(|i| i.stage);
        assert_eq!((stage("w"), stage("r")), (Some(Stage::Follow), Some(Stage::Done)));
    }

    #[test]
    fn planned_runs_and_missed_slots() {
        // hourly at :05; no run in the last two days: every slot is missed
        let snap = build(&inputs(vec![], vec![], vec![task("hourly", "5 * * * *")]), &Store::default(), 60);
        assert_eq!(snap.planned.len(), 1);
        assert_eq!(snap.planned[0].count, 36);
        assert_eq!(snap.missed.len(), 48);
        assert_eq!(snap.planned[0].title, "hourly");
    }
}
