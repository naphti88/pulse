// The notifications, as the settings say: at once when an item newly needs
// the person (by default when it is blocked or asks something); a reminder
// when an open item lies for so many hours (by default 2, 4, 12 and 24) and
// every day after; a summary each morning (by default from 8:00), or at the
// first look after it (when the Mac wakes later).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use chrono::{Datelike, Local, Timelike, Weekday};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::checks::{Check, Flag};
use crate::items::{Item, Snapshot, Stage, State};
use crate::settings::Settings;
use crate::store::{Store, Told};

const HOUR: f64 = 3_600_000.0;
// more than this many at once are told as one
const AT_ONCE: usize = 3;
// the key of the session's id among a notification's extras
pub const SESSION: &str = "session";
// how long an item or a command that a look no longer shows keeps what was
// told of it (a command also its known mark): a look that misses it, and the
// next that finds it again, tell nothing new
const GRACE: f64 = 5.0 * 60_000.0;

// since when each of those is gone, by its told id ("cmd:<key>" for a command
// and its mark); in memory only, after a restart the grace starts anew
static GONE: LazyLock<Mutex<HashMap<String, f64>>> = LazyLock::new(Default::default);

// how many reminders an item that lay for this long has earned: one per step
// passed, then one every 24 hours after the last step
pub fn reminders_for(age: f64, settings: &Settings) -> u32 {
    let hours = age / HOUR;
    let steps = settings.remind_hours.iter().filter(|&&s| hours >= s as f64).count() as u32;
    let last = settings.remind_hours.last().copied().unwrap_or(0) as f64;
    let daily = if settings.remind_daily && hours >= last + 24.0 { ((hours - last) / 24.0).floor() as u32 } else { 0 };
    steps + daily
}

// the settings changed: what lies around counts as reminded of under the new
// steps, so that a change does not send a burst of reminders
pub fn rebase(store: &mut Store, settings: &Settings, now: f64) {
    for told in store.told.values_mut() {
        told.reminders = reminders_for(now - told.since, settings);
    }
}

pub fn age_text(age: f64) -> String {
    let hours = (age / HOUR).floor() as i64;
    match hours {
        h if h < 1 => format!("{} min", (age / 60_000.0).floor() as i64),
        h if h < 24 => format!("{h} h"),
        h if h < 48 => "1 day".to_string(),
        h => format!("{} days", h / 24),
    }
}

fn label(state: State) -> &'static str {
    match state {
        State::Blocked => "Blocked",
        State::Waiting => "Needs you",
        _ => "Result to look at",
    }
}

pub fn flag_label(flag: Flag) -> &'static str {
    match flag {
        Flag::Hot => "Running hot",
        Flag::Stuck => "Stuck",
        Flag::Forgotten => "Forgotten?",
        Flag::Long => "Running long",
    }
}

fn check_line(check: &Check) -> String {
    let mut parts = vec![check.project.clone(), check.title.clone()];
    parts.extend(check.ports.iter().map(|p| format!(":{p}")));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

// a notification about one session carries its id: a click opens its chat (main.rs)
pub fn send(app: &AppHandle, title: &str, body: &str, session: Option<&str>) {
    let mut builder = app.notification().builder().title(title).body(body);
    if let Some(id) = session {
        builder = builder.extra(SESSION, id);
    }
    if let Err(error) = builder.show() {
        eprintln!("notify: {error}");
    }
}

// what is empty is left out: a Codex chat has no project
fn line(item: &Item) -> String {
    let mut parts = vec![item.project.clone(), item.reason.clone()];
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

fn summary(app: &AppHandle, snap: &Snapshot, parked_weekly: bool) {
    let c = &snap.counts;
    let open = c.blocked + c.waiting + c.review;
    let title = match open {
        0 => "Good morning: nothing open".to_string(),
        1 => "Good morning: 1 item open".to_string(),
        n => format!("Good morning: {n} items open"),
    };
    let oldest = snap.items.iter().filter(|i| i.stage == Stage::Open).min_by(|a, b| a.since.total_cmp(&b.since));
    let mut lines = Vec::new();
    for (n, one, many) in [(c.blocked, "blocked", "blocked"), (c.waiting, "question", "questions"), (c.review, "result to look at", "results to look at")] {
        if n > 0 {
            lines.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    if let Some(item) = oldest {
        lines.push(format!("Oldest: {}, open for {}", item.title, age_text(snap.now - item.since)));
    }
    let day = |t: f64| ((t + snap.tz_offset_min as f64 * 60_000.0) / HOUR / 24.0).floor();
    let planned = snap.planned.iter().filter(|p| day(p.at) == day(snap.now)).map(|p| p.count).sum::<usize>();
    if planned > 0 {
        lines.push(format!("Planned today: {planned} runs"));
    }
    // once a week what was set aside, so that it does not fall asleep
    if parked_weekly && Local::now().weekday() == Weekday::Mon && !snap.parked.is_empty() {
        let names = snap.parked.iter().take(3).map(|p| p.title.as_str()).collect::<Vec<_>>().join(", ");
        let more = if snap.parked.len() > 3 { format!(" (+{})", snap.parked.len() - 3) } else { String::new() };
        lines.push(format!("Parked: {names}{more}"));
    }
    send(app, &title, &lines.join("\n"), None);
}

// what to tell after a new snapshot; returns whether the store changed
pub fn tell(app: &AppHandle, snap: &Snapshot, store: &mut Store, settings: &Settings) -> bool {
    let mut changed = look(snap, store, settings, &mut GONE.lock().expect("gone"), |title, body, session| send(app, title, body, session));

    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    if now.hour() * 60 + now.minute() >= settings.summary_at && store.last_summary.as_deref() != Some(today.as_str()) {
        // off, the day still counts as summed up: turned on again it starts tomorrow
        if settings.summary {
            summary(app, snap, settings.parked_weekly);
        }
        store.last_summary = Some(today);
        changed = true;
    }
    changed
}

// the items and commands of a snapshot: what newly needs the person and what
// earned a reminder, handed to toast; returns whether the store changed
fn look(snap: &Snapshot, store: &mut Store, settings: &Settings, gone: &mut HashMap<String, f64>, mut toast: impl FnMut(&str, &str, Option<&str>)) -> bool {
    let open: Vec<&Item> = snap.items.iter().filter(|i| i.stage == Stage::Open).collect();
    let before = store.told.len();
    let mut changed = false;

    // on the first look what lies around is taken as told
    let is_first = !store.is_started;
    let mut fresh: Vec<&Item> = Vec::new();
    let mut reminders: Vec<&Item> = Vec::new();
    for item in &open {
        let state = format!("{:?}", item.state);
        let earned = reminders_for(snap.now - item.since, settings);
        let rule = settings.rule(item.state);
        let told = store.told.get(&item.id);
        let is_new = told.is_none_or(|t| t.since < item.since || t.state != state);
        if is_new {
            if !is_first && rule.is_some_and(|r| r.at_once) {
                fresh.push(item);
            }
            store.told.insert(item.id.clone(), Told { state, since: item.since, reminders: earned });
            changed = true;
        } else if let Some(t) = store.told.get_mut(&item.id) {
            // counted also when reminders are off, so that turning them on
            // tells only what comes from then on
            if earned > t.reminders {
                t.reminders = earned;
                if rule.is_some_and(|r| r.remind) {
                    reminders.push(item);
                }
                changed = true;
            }
        }
    }
    // the commands worth a look, as items of their own ("cmd:<key>"), told
    // from when they were first worth it
    let flagged: Vec<(&Check, Flag)> = snap.checks.iter().filter(|c| !c.known).filter_map(|c| c.flag.map(|f| (c, f))).collect();
    let mut fresh_checks: Vec<(&Check, Flag)> = Vec::new();
    let mut remind_checks: Vec<(&Check, Flag, f64)> = Vec::new();
    for &(check, flag) in &flagged {
        let id = format!("cmd:{}", check.key);
        let state = format!("{flag:?}");
        match store.told.get_mut(&id) {
            Some(t) if t.state == state => {
                let earned = reminders_for(snap.now - t.since, settings);
                if earned > t.reminders {
                    t.reminders = earned;
                    if settings.commands.remind {
                        remind_checks.push((check, flag, t.since));
                    }
                    changed = true;
                }
            }
            _ => {
                if !is_first && settings.commands.at_once {
                    fresh_checks.push((check, flag));
                }
                store.told.insert(id, Told { state, since: snap.now, reminders: 0 });
                changed = true;
            }
        }
    }
    // what the look no longer shows at all, an item or a command, keeps its
    // entries for GRACE from the first look without it. An item shown but not
    // open is not gone: one answer's second shell call asks for approval once
    // the first has run, with a since from when that came back
    let is_here = |id: &str| match id.strip_prefix("cmd:") {
        Some(key) => snap.checks.iter().any(|c| c.key == key),
        None => snap.items.iter().any(|i| i.id == id),
    };
    for id in store.told.keys().cloned().chain(store.known_at.keys().map(|key| format!("cmd:{key}"))) {
        if is_here(&id) {
            gone.remove(&id);
        } else {
            gone.entry(id).or_insert(snap.now);
        }
    }
    let is_kept = |id: &str| gone.get(id).is_some_and(|&at| snap.now - at < GRACE);

    // known is kept while the command runs
    let before_known = store.known_at.len();
    store.known_at.retain(|key, _| {
        let id = format!("cmd:{key}");
        is_here(&id) || is_kept(&id)
    });
    changed |= store.known_at.len() != before_known;

    // what is no longer open needs no reminder; an item shown but not open, or
    // a command shown but known or no longer worth a look, is told anew once it
    // is worth one again
    store.told.retain(|id, _| match id.strip_prefix("cmd:") {
        Some(key) => flagged.iter().any(|(c, _)| c.key == key) || is_kept(id),
        None => open.iter().any(|i| &i.id == id) || is_kept(id),
    });
    changed |= store.told.len() != before;
    gone.retain(|id, _| store.told.contains_key(id) || id.strip_prefix("cmd:").is_some_and(|key| store.known_at.contains_key(key)));
    if is_first {
        store.is_started = true;
        changed = true;
    }

    let n = fresh.len() + reminders.len() + fresh_checks.len() + remind_checks.len();
    if n > AT_ONCE {
        let names = fresh.iter().chain(&reminders).map(|i| i.title.as_str()).chain(fresh_checks.iter().map(|c| c.0.what.as_str())).chain(remind_checks.iter().map(|c| c.0.what.as_str())).take(4).collect::<Vec<_>>().join(", ");
        toast(&format!("{n} items need you"), &names, None);
    } else {
        for (check, flag) in &fresh_checks {
            toast(&format!("{}: {}", flag_label(*flag), check.what), &check_line(check), check.session_id.as_deref());
        }
        for (check, flag, since) in &remind_checks {
            toast(&format!("For {}: {}", age_text(snap.now - since), check.what), &format!("{} · {}", flag_label(*flag), check_line(check)), check.session_id.as_deref());
        }
        for item in &fresh {
            toast(&format!("{}: {}", label(item.state), item.title), &line(item), Some(&item.id));
        }
        for item in &reminders {
            let mut parts = vec![label(item.state).to_string(), line(item)];
            parts.retain(|p| !p.is_empty());
            toast(&format!("Open for {}: {}", age_text(snap.now - item.since), item.title), &parts.join(" · "), Some(&item.id));
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::{Counts, Kind};
    use crate::store::Known;

    const NOW: f64 = 1_792_000_000_000.0;
    const MINUTE: f64 = 60_000.0;
    const KEY: &str = "7e3c51a2-0b4d-4c8e-9f61-2a5d8b0c4e17/dev";

    fn item(state: State, stage: Stage, since: f64) -> Item {
        Item { id: "local_a".into(), title: "Fix the build".into(), project: "shop-api".into(), kind: Kind::Chat, state, stage, since, started_at: since, reason: String::new(), parent_id: None, live: None, subagents: vec![], focused: 0.0, work: vec![], why: Default::default() }
    }

    fn blocked() -> Item {
        item(State::Blocked, Stage::Open, NOW - 2.0 * MINUTE)
    }

    fn check(flag: Flag, known: bool) -> Check {
        Check { key: KEY.into(), session_id: None, title: "Fix the build".into(), project: "shop-api".into(), what: "pnpm dev".into(), background: true, flag: Some(flag), started: NOW, last_output: NOW, cpu: 0.0, memory: 0, ports: vec![], known }
    }

    // a look so many minutes after NOW: the titles it tells
    fn at(minutes: f64, items: Vec<Item>, checks: Vec<Check>, store: &mut Store, gone: &mut HashMap<String, f64>) -> Vec<String> {
        let snap = Snapshot { now: NOW + minutes * MINUTE, tz_offset_min: 0, items, planned: vec![], missed: vec![], counts: Counts::default(), checks, parked: vec![], problems: vec![] };
        let mut titles = Vec::new();
        look(&snap, store, &Settings::default(), gone, |title, _, _| titles.push(title.to_string()));
        titles
    }

    // past the first look, which takes what lies around as told
    fn started() -> (Store, HashMap<String, f64>) {
        (Store { is_started: true, ..Store::default() }, HashMap::new())
    }

    // a Codex chat has no project, and an answer that was only code no
    // reason: a body leaves out what is empty
    #[test]
    fn a_body_leaves_out_an_empty_part() {
        let (mut store, mut gone) = started();
        let mut chat = item(State::Waiting, Stage::Open, NOW);
        chat.project = String::new();
        chat.reason = "Do you mean the staging database or production?".into();
        let mut code = item(State::Review, Stage::Open, NOW);
        code.id = "codex_b".into();
        code.project = String::new();
        let mut bodies = Vec::new();
        for minutes in [0.0, 121.0] {
            let snap = Snapshot { now: NOW + minutes * MINUTE, tz_offset_min: 0, items: vec![chat.clone(), code.clone()], planned: vec![], missed: vec![], counts: Counts::default(), checks: vec![], parked: vec![], problems: vec![] };
            look(&snap, &mut store, &Settings::default(), &mut gone, |_, body, _| bodies.push(body.to_string()));
        }
        assert_eq!(bodies, ["Do you mean the staging database or production?", "Needs you · Do you mean the staging database or production?", "Result to look at"]);
    }

    #[test]
    fn a_flap_from_blocked_to_missing_to_blocked_tells_once() {
        let (mut store, mut gone) = started();
        assert_eq!(at(0.0, vec![blocked()], vec![], &mut store, &mut gone), ["Blocked: Fix the build"]);
        // missed for a look, then found again
        assert!(at(0.5, vec![], vec![], &mut store, &mut gone).is_empty());
        assert!(at(1.0, vec![blocked()], vec![], &mut store, &mut gone).is_empty());
        // a command worth a look that a look misses
        assert_eq!(at(3.0, vec![blocked()], vec![check(Flag::Stuck, false)], &mut store, &mut gone), ["Stuck: pnpm dev"]);
        assert!(at(3.5, vec![blocked()], vec![], &mut store, &mut gone).is_empty());
        assert!(at(4.0, vec![blocked()], vec![check(Flag::Stuck, false)], &mut store, &mut gone).is_empty());
        // what is new is told at once: a later since, another state, another flag
        let later = NOW + 4.2 * MINUTE;
        assert_eq!(at(4.5, vec![item(State::Blocked, Stage::Open, later)], vec![check(Flag::Stuck, false)], &mut store, &mut gone), ["Blocked: Fix the build"]);
        assert_eq!(at(5.0, vec![item(State::Waiting, Stage::Open, later)], vec![check(Flag::Hot, false)], &mut store, &mut gone), ["Running hot: pnpm dev", "Needs you: Fix the build"]);
    }

    #[test]
    fn a_second_approval_of_one_answer_is_told() {
        let (mut store, mut gone) = started();
        // two shell calls in one answer: the first waits for approval, then
        // runs, then the second waits; it is told anew even with the same since
        assert_eq!(at(0.0, vec![blocked()], vec![], &mut store, &mut gone), ["Blocked: Fix the build"]);
        assert!(at(1.0, vec![item(State::Running, Stage::Running, NOW - 2.0 * MINUTE)], vec![], &mut store, &mut gone).is_empty());
        assert!(!store.told.contains_key("local_a"));
        assert_eq!(at(1.5, vec![blocked()], vec![], &mut store, &mut gone), ["Blocked: Fix the build"]);
    }

    #[test]
    fn a_known_mark_outlasts_a_look_without_its_command() {
        let (mut store, mut gone) = started();
        assert_eq!(at(0.0, vec![], vec![check(Flag::Long, false)], &mut store, &mut gone), ["Running long: pnpm dev"]);
        // marked as known: what was told of it goes at once, as it shows
        store.known_at.insert(KEY.into(), Known { flag: Some(Flag::Long), at: NOW });
        assert!(at(0.5, vec![], vec![check(Flag::Long, true)], &mut store, &mut gone).is_empty());
        assert!(!store.told.contains_key(&format!("cmd:{KEY}")));
        // a look misses it: the mark stays, and found again it is still known
        assert!(at(1.0, vec![], vec![], &mut store, &mut gone).is_empty());
        assert!(store.known_at.contains_key(KEY));
        assert!(at(1.5, vec![], vec![check(Flag::Long, true)], &mut store, &mut gone).is_empty());
        assert!(store.known_at.contains_key(KEY));
    }

    #[test]
    fn what_stays_gone_is_dropped_after_the_grace() {
        let (mut store, mut gone) = started();
        store.known_at.insert(KEY.into(), Known { flag: Some(Flag::Long), at: NOW });
        assert_eq!(at(0.0, vec![blocked()], vec![check(Flag::Long, true)], &mut store, &mut gone), ["Blocked: Fix the build"]);
        // gone from the look at 1 min: kept until 5 min after it
        assert!(at(1.0, vec![], vec![], &mut store, &mut gone).is_empty());
        assert!(at(5.5, vec![], vec![], &mut store, &mut gone).is_empty());
        assert!(store.told.contains_key("local_a") && store.known_at.contains_key(KEY));
        assert!(at(6.0, vec![], vec![], &mut store, &mut gone).is_empty());
        assert!(store.told.is_empty() && store.known_at.is_empty() && gone.is_empty());
        // back after that, it is told as new
        assert_eq!(at(7.0, vec![blocked()], vec![], &mut store, &mut gone), ["Blocked: Fix the build"]);
    }

    #[test]
    fn reminders_come_after_2_4_12_24_hours_and_then_daily() {
        let h = |n: f64| n * HOUR;
        let s = Settings::default();
        assert_eq!(reminders_for(h(1.9), &s), 0);
        assert_eq!(reminders_for(h(2.0), &s), 1);
        assert_eq!(reminders_for(h(5.0), &s), 2);
        assert_eq!(reminders_for(h(12.5), &s), 3);
        assert_eq!(reminders_for(h(30.0), &s), 4);
        assert_eq!(reminders_for(h(48.0), &s), 5);
        assert_eq!(reminders_for(h(73.0), &s), 6);
        // chosen steps, without the daily ones
        let own = Settings { remind_hours: vec![1, 8], remind_daily: false, ..Settings::default() };
        assert_eq!(reminders_for(h(0.5), &own), 0);
        assert_eq!(reminders_for(h(9.0), &own), 2);
        assert_eq!(reminders_for(h(100.0), &own), 2);
        // no steps, daily only: every 24 hours
        let daily = Settings { remind_hours: vec![], ..Settings::default() };
        assert_eq!((reminders_for(h(23.0), &daily), reminders_for(h(49.0), &daily)), (0, 2));
        assert_eq!(age_text(h(5.5)), "5 h");
        assert_eq!(age_text(h(50.0)), "2 days");
    }
}
