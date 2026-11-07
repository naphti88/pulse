// What is open, as Pulse decided it, for a reader outside the app: the skill
// /roundup (tools/roundup) reads this file, so that it needs no rules of its
// own and sees the person's marks. It stays on this Mac beside the state.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::checks::Check;
use crate::items::{Item, Kind, Missed, ParkedView, Planned, Snapshot, Stage, State};
use crate::rules::limits::CLOSED_OPEN_MS;
use crate::store::{MarkKind, Store};
use crate::subagents::SubagentView;
use crate::util::home;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry<'a> {
    id: &'a str,
    title: &'a str,
    project: &'a str,
    kind: Kind,
    state: State,
    stage: Stage,
    since: f64,
    started_at: f64,
    reason: &'a str,
    parent_id: Option<&'a str>,
    work: &'a [String],
    subagents: Vec<&'a SubagentView>,
    // when the person closed it, with its question unanswered
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<f64>,
}

impl<'a> Entry<'a> {
    fn of(item: &'a Item, closed_at: Option<f64>) -> Self {
        Entry {
            id: &item.id,
            title: &item.title,
            project: &item.project,
            kind: item.kind,
            state: item.state,
            stage: item.stage,
            since: item.since,
            started_at: item.started_at,
            reason: &item.reason,
            parent_id: item.parent_id.as_deref(),
            work: &item.work,
            subagents: item.subagents.iter().filter(|a| a.is_running).collect(),
            closed_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Export<'a> {
    written_at: f64,
    tz_offset_min: i64,
    // running, open and set aside with "later"
    open: Vec<Entry<'a>>,
    // closed by the person although its last answer still asked something
    closed_open: Vec<Entry<'a>>,
    parked: &'a [ParkedView],
    planned: &'a [Planned],
    missed: &'a [Missed],
    commands: Vec<&'a Check>,
    problems: &'a [String],
}

fn closed_at(item: &Item, store: &Store, now: f64) -> Option<f64> {
    if item.stage != Stage::Done || !matches!(item.state, State::Waiting | State::Blocked) {
        return None;
    }
    let mark = store.marks.get(&item.id).filter(|m| m.kind == MarkKind::Done)?;
    (mark.at > now - CLOSED_OPEN_MS).then_some(mark.at)
}

fn export<'a>(snap: &'a Snapshot, store: &Store) -> Export<'a> {
    Export {
        written_at: snap.now,
        tz_offset_min: snap.tz_offset_min,
        open: snap.items.iter().filter(|i| matches!(i.stage, Stage::Running | Stage::Open | Stage::Follow)).map(|i| Entry::of(i, None)).collect(),
        closed_open: snap.items.iter().filter_map(|i| closed_at(i, store, snap.now).map(|at| Entry::of(i, Some(at)))).collect(),
        parked: &snap.parked,
        planned: &snap.planned,
        missed: &snap.missed,
        commands: snap.checks.iter().filter(|c| c.flag.is_some() && !c.known).collect(),
        problems: &snap.problems,
    }
}

fn path() -> Result<PathBuf, String> {
    Ok(home()?.join("Library/Application Support/local.claude-dashboard.pulse/open-items.json"))
}

fn write_to(snap: &Snapshot, store: &Store, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&export(snap, store)).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn write(snap: &Snapshot, store: &Store) -> Result<(), String> {
    write_to(snap, store, &path()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::Counts;
    use crate::rules::Why;
    use crate::store::Mark;

    const NOW: f64 = 1_792_000_000_000.0;
    const DAY: f64 = 86_400_000.0;

    fn item(id: &str, state: State, stage: Stage) -> Item {
        Item {
            id: id.into(),
            title: format!("Chat {id}"),
            project: "shop-api".into(),
            kind: Kind::Chat,
            state,
            stage,
            since: NOW - DAY,
            started_at: NOW - 2.0 * DAY,
            reason: "Soll ich das einbauen?".into(),
            parent_id: None,
            live: None,
            subagents: Vec::new(),
            focused: 0.0,
            work: Vec::new(),
            why: Why::default(),
        }
    }

    fn snapshot(items: Vec<Item>) -> Snapshot {
        Snapshot { now: NOW, tz_offset_min: 60, items, planned: Vec::new(), missed: Vec::new(), counts: Counts::default(), checks: Vec::new(), parked: Vec::new(), problems: Vec::new() }
    }

    fn done(at: f64) -> Mark {
        Mark { kind: MarkKind::Done, at, shown: None }
    }

    #[test]
    fn what_is_open_goes_out_and_what_is_settled_does_not() {
        let snap = snapshot(vec![
            item("asks", State::Waiting, Stage::Open),
            item("later", State::Waiting, Stage::Follow),
            item("runs", State::Running, Stage::Running),
            item("read", State::Review, Stage::Done),
        ]);
        let out = export(&snap, &Store::default());
        assert_eq!(out.open.iter().map(|e| e.id).collect::<Vec<_>>(), ["asks", "later", "runs"]);
        assert!(out.closed_open.is_empty());
    }

    #[test]
    fn a_closed_question_is_named_for_seven_days() {
        let snap = snapshot(vec![item("fresh", State::Waiting, Stage::Done), item("old", State::Blocked, Stage::Done), item("answered", State::Review, Stage::Done)]);
        let mut store = Store::default();
        store.marks.insert("fresh".into(), done(NOW - 6.0 * DAY));
        store.marks.insert("old".into(), done(NOW - 8.0 * DAY));
        store.marks.insert("answered".into(), done(NOW - DAY));
        let out = export(&snap, &store);
        assert_eq!(out.closed_open.iter().map(|e| e.id).collect::<Vec<_>>(), ["fresh"]);
        assert_eq!(out.closed_open[0].closed_at, Some(NOW - 6.0 * DAY));
    }

    #[test]
    fn the_file_holds_what_the_skill_reads() {
        let dir = std::env::temp_dir().join(format!("pulse-export-{}", std::process::id()));
        let file = dir.join("open-items.json");
        write_to(&snapshot(vec![item("asks", State::Waiting, Stage::Open)]), &Store::default(), &file).expect("write");
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
        assert_eq!(json["open"][0]["id"], "asks");
        assert_eq!(json["open"][0]["stage"], "open");
        assert_eq!(json["writtenAt"], NOW);
        assert!(json["open"][0].get("closedAt").is_none());
        std::fs::remove_dir_all(&dir).expect("clean");
    }
}
