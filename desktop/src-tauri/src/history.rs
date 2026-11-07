// Verlauf: every session the app still keeps, archived ones too, by its last
// activity, to find one again and go back to it. What each said last comes
// with it: the STATUS of a chat's last answer, a routine run's STATUS, else the
// app's own summary. Worked out only when the panel asks for it.

use std::collections::HashMap;

use serde::Serialize;

use crate::inputs::{transcript_of, Inputs, Session};
use crate::items::{clean_name, kind_of, project_of, Item, Kind, Stage, State};
use crate::said::said_of;
use crate::util::paths;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub project: String,
    pub kind: Kind,
    pub at: f64,
    // as the panel shows the item now; one no longer shown is done
    pub state: State,
    pub stage: Stage,
    pub said: String,
    pub archived: bool,
    // the session that started this one, when Claude did
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

pub fn history(input: &Inputs, items: &[Item]) -> Vec<Entry> {
    entries(input, items, |_| true)
}

// Park needs only the requested sessions. Filter before reading any old rollout:
// one parked thread must not decompress the rest of the history.
pub fn selected(input: &Inputs, items: &[Item], ids: &[&str]) -> Vec<Entry> {
    entries(input, items, |s| ids.contains(&s.id.as_str()))
}

fn entries(input: &Inputs, items: &[Item], include: impl Fn(&Session) -> bool) -> Vec<Entry> {
    let projects = paths().map(|p| p.projects).ok();
    let tasks: HashMap<&str, _> = input.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let records: HashMap<&str, _> = input.records.iter().map(|r| (r.session_id.as_str(), r)).collect();
    let mut list: Vec<Entry> = input
        .sessions
        .iter()
        .filter(|s| include(s))
        .map(|s| {
            let item = items.iter().find(|i| i.id == s.id);
            let task = s.task_id.as_deref().and_then(|id| tasks.get(id).copied());
            let record = s.cli_id.as_deref().and_then(|id| records.get(id));
            let kind = if s.task_id.is_some() || record.is_some() { kind_of(task) } else { Kind::Chat };
            let title = match (item, task, &s.title) {
                (Some(i), _, _) => i.title.clone(),
                (None, Some(t), _) => clean_name(&t.name),
                (None, None, Some(title)) => title.clone(),
                (None, None, None) => s.task_id.as_deref().map(clean_name).unwrap_or_else(|| s.id.clone()),
            };
            let folder = s.origin_cwd.as_deref().or(s.cwd.as_deref()).or(task.and_then(|t| t.cwd.as_deref())).unwrap_or("");
            // what the run's last turn said: an earlier turn's sentence is out of date
            let status = record.and_then(|r| r.turns.last()).and_then(|t| t.status_text.clone());
            let resting = s.rollout.as_deref().and_then(crate::codex::said_at_rest);
            // a chat's last answer, read from its transcript (cached by size)
            let said = || {
                if let Some(summary) = &resting {
                    return Some(summary.text.clone()).filter(|text| !text.is_empty());
                }
                let cli = s.cli_id.as_deref()?;
                let path = transcript_of(projects.as_deref()?, cli, s.cwd.as_deref())?;
                said_of(&path).map(|said| said.text).filter(|t| !t.is_empty())
            };
            let said = s.summary.words(status.or_else(|| if kind == Kind::Chat { said() } else { None }));
            Entry {
                id: s.id.clone(),
                title,
                project: project_of(folder),
                kind,
                at: resting.as_ref().and_then(|summary| summary.at).or(s.last_activity).or(s.created_at).unwrap_or(0.0),
                state: item.map_or(State::Quiet, |i| i.state),
                stage: item.map_or(Stage::Done, |i| i.stage),
                said,
                archived: s.archived,
                parent_id: s.parent_id.clone(),
            }
        })
        .collect();
    list.sort_by(|a, b| b.at.total_cmp(&a.at));
    list
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::inputs::Session;
    use crate::runs::{RunRecord, Turn};

    #[test]
    fn newest_first_archived_too_and_what_the_app_said() {
        let mut input = Inputs::empty(1_000.0);
        let mut old = Session { id: "a".into(), title: Some("Alt".into()), cwd: Some("/w/shop-api".into()), archived: true, last_activity: Some(100.0), ..Default::default() };
        old.summary.detail = "PR #12 gemergt".into();
        let new = Session { id: "b".into(), title: Some("Neu".into()), cwd: Some("/w/website".into()), last_activity: Some(900.0), task_id: Some("news".into()), ..Default::default() };
        input.sessions = vec![old, new];
        let list = history(&input, &[]);
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!((list[0].kind, list[0].project.as_str()), (Kind::Recurring, "website"));
        assert_eq!((list[1].archived, list[1].said.as_str(), list[1].stage), (true, "PR #12 gemergt", Stage::Done));
    }

    #[test]
    fn a_run_says_what_its_last_turn_said_or_what_the_app_did() {
        let mut input = Inputs::empty(1_000.0);
        let mut run = Session { id: "r".into(), cli_id: Some("c".into()), task_id: Some("ops".into()), last_activity: Some(900.0), ..Default::default() };
        run.summary.detail = "App-Zusammenfassung".into();
        input.sessions = vec![run];
        let turn = |by: &str, status: &str, text: Option<&str>| Turn { by: by.into(), status: Some(status.into()), status_text: text.map(Into::into), ..Default::default() };
        let record = |last: Turn| Arc::new(RunRecord { session_id: "c".into(), task_id: "ops".into(), turns: vec![turn("routine", "waiting", Some("Soll ich die Dateien löschen?")), last], ..Default::default() });
        // the question was answered; the run is blocked now and said no sentence
        input.records = vec![record(turn("user", "blocked", None))];
        assert_eq!(history(&input, &[])[0].said, "App-Zusammenfassung");
        // the person wrote again and the run has set no state since
        input.records = vec![record(Turn { by: "user".into(), ..Default::default() })];
        assert_eq!(history(&input, &[])[0].said, "App-Zusammenfassung");
        input.records = vec![record(turn("user", "worked", Some("Gelöscht.")))];
        assert_eq!(history(&input, &[])[0].said, "Gelöscht.");
    }
}
