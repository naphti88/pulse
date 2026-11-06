// The notifications, as the settings say: at once when an item newly needs
// the person (by default when it is blocked or asks something); a reminder
// when an open item lies for so many hours (by default 2, 4, 12 and 24) and
// every day after; a summary each morning (by default from 8:00), or at the
// first look after it (when the Mac wakes later).

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
fn send(app: &AppHandle, title: &str, body: &str, session: Option<&str>) {
    let mut builder = app.notification().builder().title(title).body(body);
    if let Some(id) = session {
        builder = builder.extra(SESSION, id);
    }
    if let Err(error) = builder.show() {
        eprintln!("notify: {error}");
    }
}

fn line(item: &Item) -> String {
    if item.reason.is_empty() {
        item.project.clone()
    } else {
        format!("{} · {}", item.project, item.reason)
    }
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
    let mut changed = look(snap, store, settings, |title, body, session| send(app, title, body, session));
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

// what is new or due for a reminder, each through toast; returns whether the store changed
fn look(snap: &Snapshot, store: &mut Store, settings: &Settings, mut toast: impl FnMut(&str, &str, Option<&str>)) -> bool {
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
    // known is kept while the command runs
    let before_known = store.known_at.len();
    store.known_at.retain(|key, _| snap.checks.iter().any(|c| &c.key == key));
    changed |= store.known_at.len() != before_known;

    // what is no longer open needs no reminder
    store.told.retain(|id, _| open.iter().any(|i| &i.id == id) || id.strip_prefix("cmd:").is_some_and(|key| flagged.iter().any(|(c, _)| c.key == key)));
    changed |= store.told.len() != before;
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
            toast(&format!("Open for {}: {}", age_text(snap.now - item.since), item.title), &format!("{} · {}", label(item.state), line(item)), Some(&item.id));
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn started() -> Store {
        Store { is_started: true, ..Store::default() }
    }

    #[test]
    fn pending_transcripts_keep_their_reminders_across_refreshes_and_restart() {
        use crate::inputs::tests::{PendingFixture, PENDING_AT};

        let fixture = PendingFixture::new("Bash");
        let mut store = started();
        let mut check = |fixture: &PendingFixture, hours: f64, expected: usize| {
            let input = fixture.load(PENDING_AT + hours * HOUR);
            let snap = crate::items::build(&input, &store, 0);
            let mut titles = Vec::new();
            look(&snap, &mut store, &Settings::default(), |title, _, _| titles.push(title.to_string()));
            assert_eq!(titles.len(), expected, "at {hours} hours: {titles:?}");
            if hours < 7.0 * 24.0 {
                assert_eq!((snap.items[0].state, snap.items[0].stage, snap.items[0].since), (State::Blocked, Stage::Open, PENDING_AT));
            }
        };
        check(&fixture, 0.5 / 60.0, 1);
        check(&fixture, 31.0 / 60.0, 0);
        for hours in [2.0, 4.0, 12.0, 24.0, 48.0, 72.0, 144.0] {
            check(&fixture, hours, 1);
            check(&fixture, hours + 0.01, 0);
        }
        // New transcript path forces a cold read; persisted notification state
        // must avoid a new Blocked toast when Pulse starts again.
        let restarted = PendingFixture::new("Bash");
        let mut store: Store = serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        let snap = crate::items::build(&restarted.load(PENDING_AT + 145.0 * HOUR), &store, 0);
        let mut titles = Vec::new();
        look(&snap, &mut store, &Settings::default(), |title, _, _| titles.push(title.to_string()));
        assert!(titles.is_empty());
        let expired = crate::items::build(&restarted.load(PENDING_AT + 168.0 * HOUR), &store, 0);
        look(&expired, &mut store, &Settings::default(), |title, _, _| titles.push(title.to_string()));
        assert!(titles.is_empty());
        assert_eq!(expired.items[0].stage, Stage::Done);
        assert!(!store.told.contains_key("local_pending"));
    }
}
