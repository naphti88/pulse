// The commands the sessions run now and which of them are worth a look, by the
// limits in the settings: one that spins (most of a core for long), one a
// session waits for too long in the foreground, one in the background whose
// session went quiet (forgotten: a dev server, an emulator, a tunnel), one in
// the background that simply runs long. What the person marked as known stays
// listed but is not counted, until the next reminder step of its flag (by
// default 2, 4, 12 and 24 hours after it became worth a look, then daily) or
// until it is worth a look for another reason.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::inputs::{Inputs, Session};
use crate::items::{project_of, Item};
use crate::notify::reminders_for;
use crate::settings::Settings;
use crate::store::Store;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Flag {
    Hot,
    Stuck,
    Forgotten,
    Long,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    // "<session>/<id>", the session's id in the transcript's name
    pub key: String,
    // the app's id of the session, to open its chat
    pub session_id: Option<String>,
    pub title: String,
    pub project: String,
    pub what: String,
    pub background: bool,
    pub flag: Option<Flag>,
    pub started: f64,
    pub last_output: f64,
    pub cpu: f64,
    pub memory: u64,
    pub ports: Vec<u16>,
    pub known: bool,
}

const MINUTE: f64 = 60_000.0;

pub fn checks(input: &Inputs, items: &[Item], settings: &Settings, store: &Store) -> Vec<Check> {
    let now = input.now;
    let limits = &settings.limits;
    let sessions: HashMap<&str, &Session> = input.sessions.iter().filter_map(|s| s.cli_id.as_deref().map(|c| (c, s))).collect();
    let mut list: Vec<Check> = input
        .commands
        .iter()
        .map(|c| {
            let session = sessions.get(c.session.as_str()).copied();
            let item = session.and_then(|s| items.iter().find(|i| i.id == s.id));
            let names = input.names.get(&c.session);
            let named = names.and_then(|n| n.get(&c.id));
            // in the background: so named by the transcript, or left running
            // by a call that returned
            let background = named.is_some() || c.left;
            let what = match named {
                Some(what) if !what.is_empty() => what.clone(),
                Some(_) => "Command".to_string(),
                None if c.left => "Command left running".to_string(),
                // in the foreground: the call the session waits for, while in
                // its turn (after it, what it waits for is the background)
                None => session.filter(|s| input.in_turn.contains(&s.id)).and_then(|s| input.live.get(&s.id)).and_then(|l| l.tool.clone()).unwrap_or_else(|| "Command in the foreground".to_string()),
            };
            let quiet_since = input.written.get(&c.session).copied().unwrap_or(c.started);
            let is_gone = session.is_none_or(|s| s.archived);
            // what it is worth a look for, and since when
            let hot = c.hot_since.map(|h| h + limits.hot_min as f64 * MINUTE);
            let stuck = c.started + limits.stuck_min as f64 * MINUTE;
            let forgotten = if is_gone { quiet_since } else { quiet_since + limits.forgotten_min as f64 * MINUTE };
            let long = c.started + limits.long_min as f64 * MINUTE;
            let (flag, flagged_at) = match hot {
                Some(at) if now >= at => (Some(Flag::Hot), at),
                _ if !background && now >= stuck => (Some(Flag::Stuck), stuck),
                _ if background && now >= forgotten => (Some(Flag::Forgotten), forgotten),
                _ if background && now >= long => (Some(Flag::Long), long),
                _ => (None, now),
            };
            let key = format!("{}/{}", c.session, c.id);
            // known for the flag it has and the reminder step it is in
            let known = store.known_at.get(&key).is_some_and(|k| k.flag == flag && reminders_for(k.at - flagged_at, settings) >= reminders_for(now - flagged_at, settings));
            Check {
                known,
                key,
                session_id: session.map(|s| s.id.clone()),
                title: item.map(|i| i.title.clone()).or_else(|| session.and_then(|s| s.title.clone())).unwrap_or_else(|| format!("Session {}", &c.session[..8])),
                project: item.map(|i| i.project.clone()).or_else(|| session.and_then(|s| s.cwd.as_deref()).map(project_of)).unwrap_or_default(),
                what,
                background,
                flag,
                started: c.started,
                last_output: c.last_output,
                cpu: c.cpu,
                memory: c.memory,
                ports: c.ports.clone(),
            }
        })
        .collect();
    list.sort_by(|a, b| (a.flag.is_none(), a.flag, a.started).partial_cmp(&(b.flag.is_none(), b.flag, b.started)).expect("no NaN"));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::Command;
    use crate::store::Known;

    const NOW: f64 = 1_792_000_000_000.0;
    const CLI: &str = "7e3c51a2-0b4d-4c8e-9f61-2a5d8b0c4e17";

    fn command(id: &str, minutes: f64) -> Command {
        Command { session: CLI.into(), id: id.into(), started: NOW - minutes * MINUTE, last_output: NOW, cpu: 0.0, hot_since: None, memory: 1, ports: vec![], left: false }
    }

    fn flags(input: &Inputs) -> Vec<(String, Option<Flag>)> {
        let mut list: Vec<_> = checks(input, &[], &Settings::default(), &Store::default()).into_iter().map(|c| (c.key[37..].to_string(), c.flag)).collect();
        list.sort();
        list
    }

    #[test]
    fn long_forgotten_stuck_and_hot() {
        let mut input = Inputs::empty(NOW);
        input.commands = vec![
            command("dev", 30.0),     // background, session active: nothing
            command("watch", 130.0),  // background, past 2 h
            command("tests", 20.0),   // foreground, past 15 min
            command("quick", 5.0),    // foreground, fine
            Command { hot_since: Some(NOW - 11.0 * MINUTE), ..command("loop", 30.0) },
        ];
        input.names.insert(CLI.into(), HashMap::from([("dev".into(), "pnpm dev".into()), ("watch".into(), "Watcher".into()), ("loop".into(), String::new())]));
        input.written.insert(CLI.into(), NOW - 10.0 * MINUTE);
        // no session in the app for it: gone, its background work forgotten
        assert_eq!(
            flags(&input),
            vec![
                ("dev".into(), Some(Flag::Forgotten)),
                ("loop".into(), Some(Flag::Hot)),
                ("quick".into(), None),
                ("tests".into(), Some(Flag::Stuck)),
                ("watch".into(), Some(Flag::Forgotten)),
            ]
        );

        let mut session = Session::default();
        session.id = "local_x".into();
        session.cli_id = Some(CLI.into());
        input.sessions = vec![session];
        let with_session = flags(&input);
        assert_eq!(with_session.iter().find(|(id, _)| id == "dev").map(|f| f.1), Some(None));
        assert_eq!(with_session.iter().find(|(id, _)| id == "watch").map(|f| f.1), Some(Some(Flag::Long)));

        // quiet for over an hour: what runs in the background is forgotten
        input.written.insert(CLI.into(), NOW - 61.0 * MINUTE);
        assert_eq!(flags(&input).iter().find(|(id, _)| id == "dev").map(|f| f.1), Some(Some(Flag::Forgotten)));
    }

    // what a call left running as it returned: no one waits for it, so it is
    // background work (running long, forgotten once its session is quiet,
    // never stuck), told apart from the call the session runs now
    #[test]
    fn what_a_call_left_running_is_background_work() {
        let mut input = Inputs::empty(NOW);
        let mut session = Session::default();
        session.id = "local_x".into();
        session.cli_id = Some(CLI.into());
        input.sessions = vec![session];
        let mut live = crate::live::Live::default();
        live.tool = Some("Bash · npm test".into());
        input.live.insert("local_x".into(), live);
        input.in_turn.insert("local_x".into());
        input.commands = vec![Command { left: true, ..command("server", 30.0) }, Command { left: true, ..command("old", 130.0) }, command("tests", 20.0)];
        input.written.insert(CLI.into(), NOW - 10.0 * MINUTE);
        assert_eq!(flags(&input), vec![("old".into(), Some(Flag::Long)), ("server".into(), None), ("tests".into(), Some(Flag::Stuck))]);
        let list = checks(&input, &[], &Settings::default(), &Store::default());
        let what: Vec<(&str, bool)> = list.iter().map(|c| (c.what.as_str(), c.background)).collect();
        assert_eq!(what, [("Bash · npm test", false), ("Command left running", true), ("Command left running", true)]);
        input.written.insert(CLI.into(), NOW - 61.0 * MINUTE);
        assert_eq!(flags(&input), vec![("old".into(), Some(Flag::Forgotten)), ("server".into(), Some(Flag::Forgotten)), ("tests".into(), Some(Flag::Stuck))]);
        // one the transcript names keeps its name
        input.names.insert(CLI.into(), HashMap::from([("server".into(), "pnpm dev".into())]));
        let list = checks(&input, &[], &Settings::default(), &Store::default());
        assert!(list.iter().any(|c| c.key.ends_with("/server") && c.what == "pnpm dev" && c.background));
    }

    // a command of a session whose turn has ended, that the transcript does
    // not name (a subagent's, say): not labelled with what the session now
    // waits for in the background
    #[test]
    fn a_command_after_the_turn_is_not_the_background_work() {
        let mut input = Inputs::empty(NOW);
        let mut session = Session::default();
        session.id = "local_x".into();
        session.cli_id = Some(CLI.into());
        input.sessions = vec![session];
        let mut live = crate::live::Live::default();
        live.tool = Some("in the background · workflow (+1)".into());
        input.live.insert("local_x".into(), live);
        input.commands = vec![command("tests", 20.0)];
        let what = |input: &Inputs| checks(input, &[], &Settings::default(), &Store::default())[0].what.clone();
        assert_eq!(what(&input), "Command in the foreground");
        // while in its turn, it is the call the session waits for
        input.live.get_mut("local_x").expect("live").tool = Some("Bash · npm test".into());
        input.in_turn.insert("local_x".into());
        assert_eq!(what(&input), "Bash · npm test");
    }

    #[test]
    fn known_holds_until_the_next_step() {
        let mut session = Session::default();
        session.id = "local_x".into();
        session.cli_id = Some(CLI.into());
        let at = |minutes: f64| {
            let mut input = Inputs::empty(NOW + minutes * MINUTE);
            input.sessions = vec![session.clone()];
            input.commands = vec![command("watch", 130.0)];
            input.names.insert(CLI.into(), HashMap::from([("watch".into(), "Watcher".into())]));
            input.written.insert(CLI.into(), NOW + minutes * MINUTE);
            input
        };
        // running long since 2 h, marked known after 2 h 10 min
        let key = format!("{CLI}/watch");
        let mut store = Store::default();
        store.known_at.insert(key.clone(), Known { flag: Some(Flag::Long), at: NOW });
        let known = |input: &Inputs, store: &Store| checks(input, &[], &Settings::default(), store)[0].known;
        assert!(known(&at(0.0), &store));
        // the first step, 2 h after it became worth a look: shown again
        assert!(known(&at(109.0), &store));
        assert!(!known(&at(111.0), &store));
        // known anew, it holds until the next step, at 4 h
        store.known_at.insert(key.clone(), Known { flag: Some(Flag::Long), at: NOW + 111.0 * MINUTE });
        assert!(known(&at(229.0), &store));
        assert!(!known(&at(231.0), &store));
        // worth a look for another reason: shown again at once
        let mut quiet = at(0.0);
        quiet.written.insert(CLI.into(), NOW - 61.0 * MINUTE);
        assert!(!known(&quiet, &store));
    }
}
