// Pulse: a menu bar item that shows what the Claude app's sessions and
// routines are doing and what of it needs the person, with a panel below it
// and notifications. Everything is worked out here; the panel only draws.

mod background;
mod checks;
mod cron;
mod drift;
mod engine;
mod failed;
mod front;
mod history;
mod inputs;
mod items;
mod live;
mod notify;
mod panel;
mod pending;
mod procs;
mod rules;
mod running;
mod runs;
mod said;
mod scrub;
mod settings;
mod store;
mod subagents;
mod tray;
mod unread;
mod util;
mod watch;
mod work;

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};

use chrono::Local;
use tauri::{ActivationPolicy, AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt;

use crate::items::Snapshot;
use crate::settings::{Settings, SETTINGS};
use crate::store::{save, Known, Mark, MarkKind, Parked, STORE};
use crate::util::now_ms;

static LAST: LazyLock<Mutex<Option<Snapshot>>> = LazyLock::new(Default::default);
// one refresh at a time: the watcher, the tick and the panel's actions all start one
static REFRESHING: Mutex<()> = Mutex::new(());

fn tz_offset_min() -> i64 {
    Local::now().offset().local_minus_utc() as i64 / 60
}

fn compute() -> Result<Snapshot, String> {
    let inputs = inputs::load_inputs()?;
    let mut store = STORE.lock().expect("store");
    let mut snap = items::build(&inputs, &store, tz_offset_min());
    let before = LAST.lock().expect("last").as_ref().map(|s| s.items.clone()).unwrap_or_default();
    if items::sync_unseen(&snap.items, &before, &mut store, front::claude_in_front()) {
        snap = items::build(&inputs, &store, tz_offset_min());
        if let Err(error) = save(&store) {
            eprintln!("store: {error}");
        }
    }
    let (settings, problem) = SETTINGS.lock().expect("settings").clone();
    if let Some(problem) = problem {
        snap.problems.push(format!("Settings unreadable, the defaults apply: {problem}"));
    }
    snap.problems.extend(store::problem());
    snap.checks = checks::checks(&inputs, &snap.items, &settings, &store);
    snap.counts.commands = snap.checks.iter().filter(|c| c.flag.is_some() && !c.known).count();
    Ok(snap)
}

// read, work out, show in the menu bar, tell, and hand to the panel
fn refresh(app: &AppHandle) {
    let _one = REFRESHING.lock().expect("refresh");
    let snap = match compute() {
        Ok(snap) => snap,
        Err(error) => {
            eprintln!("refresh: {error}");
            return;
        }
    };
    tray::show(app, &snap.counts);
    {
        let mut store = STORE.lock().expect("store");
        let settings = SETTINGS.lock().expect("settings").0.clone();
        let parked = items::sync_parked(&snap, &mut store);
        if notify::tell(app, &snap, &mut store, &settings) || parked {
            if let Err(error) = save(&store) {
                eprintln!("store: {error}");
            }
        }
    }
    // a hidden panel gets the snapshot when it is shown again
    if panel::is_visible(app) {
        emit(app, &snap);
    }
    *LAST.lock().expect("last") = Some(snap);
}

fn emit(app: &AppHandle, snap: &Snapshot) {
    if let Err(error) = app.emit_to(panel::LABEL, "snapshot", snap) {
        eprintln!("emit: {error}");
    }
}

// the panel is shown: hand it the latest snapshot
pub fn push_last(app: &AppHandle) {
    if let Some(snap) = LAST.lock().expect("last").as_ref() {
        emit(app, snap);
    }
}

// an app id as the app writes it: local_ and a UUID
fn is_app_id(id: &str) -> bool {
    id.len() == 42 && id.starts_with("local_") && id[6..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-'))
}

#[tauri::command]
async fn snapshot() -> Result<Snapshot, String> {
    if let Some(snap) = LAST.lock().expect("last").clone() {
        return Ok(snap);
    }
    tauri::async_runtime::spawn_blocking(compute).await.map_err(|e| e.to_string())?
}

// seen moves an item to follow up, done closes it, open takes the mark back;
// park sets a topic aside with a copy of what it is, resume takes it up again,
// forget drops the note of one whose session is gone
#[tauri::command]
async fn mark(app: AppHandle, ids: Vec<String>, kind: String, shown: Option<f64>) -> Result<(), String> {
    if ids.len() > 500 || ids.iter().any(|id| !is_app_id(id)) {
        return Err("ids".into());
    }
    {
        let last = LAST.lock().expect("last").clone();
        let mut store = STORE.lock().expect("store");
        for id in ids {
            match kind.as_str() {
                "seen" => {
                    store.marks.insert(id, Mark { kind: MarkKind::Seen, at: now_ms(), shown });
                }
                "done" => {
                    store.marks.insert(id, Mark { kind: MarkKind::Done, at: now_ms(), shown });
                }
                "open" => {
                    store.marks.remove(&id);
                }
                "park" => {
                    let item = last.as_ref().and_then(|s| s.items.iter().find(|i| i.id == id));
                    let entry = if item.is_none() { history_entry(&id) } else { None };
                    let (title, project, said) = match (item, entry) {
                        (Some(i), _) => (i.title.clone(), i.project.clone(), i.reason.clone()),
                        (None, Some(e)) => (e.title, e.project, e.said),
                        (None, None) => return Err("unknown session".into()),
                    };
                    store.resumed.remove(&id);
                    store.parked.insert(id, Parked { at: now_ms(), title, project, said });
                }
                "resume" => {
                    store.parked.remove(&id);
                    store.resumed.insert(id, now_ms());
                }
                "forget" => {
                    store.parked.remove(&id);
                }
                _ => return Err("kind".into()),
            }
        }
        save(&store)?;
    }
    tauri::async_runtime::spawn_blocking(move || refresh(&app)).await.map_err(|e| e.to_string())
}

fn open(args: &[&str]) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").args(args).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open: {status}"))
    }
}

#[tauri::command]
fn open_chat(app: AppHandle, id: String) -> Result<(), String> {
    if !is_app_id(&id) {
        return Err("id".into());
    }
    if !panel::PINNED.load(Ordering::SeqCst) {
        panel::hide(&app);
    }
    STORE.lock().expect("store").unseen.remove(&id);
    open(&[&format!("claude://claude.ai/epitaxy/{id}")])
}

// keeps the shape of what an item is now as a case, on this Mac (scrub.rs)
#[tauri::command]
async fn save_case(id: String) -> Result<String, String> {
    if !is_app_id(&id) {
        return Err("id".into());
    }
    let item = LAST.lock().expect("last").as_ref().and_then(|s| s.items.iter().find(|i| i.id == id).cloned()).ok_or("unknown session")?;
    tauri::async_runtime::spawn_blocking(move || {
        let inputs = inputs::load_inputs()?;
        let session = inputs.sessions.iter().find(|s| s.id == id).ok_or("unknown session")?;
        let cli_id = session.cli_id.as_deref().ok_or("no transcript")?;
        let path = inputs::transcript_of(&util::paths()?.projects, cli_id, session.cwd.as_deref()).ok_or("no transcript")?;
        let has_process = inputs.commands.iter().any(|c| c.session == cli_id);
        scrub::save_case(&path, &item, has_process, inputs.engines.by_id.get(cli_id), inputs.stopped.get(cli_id), inputs.now).map(|saved| saved.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// a session the panel does not show now (archived, long quiet), as the Verlauf has it
fn history_entry(id: &str) -> Option<history::Entry> {
    let inputs = inputs::load_inputs().ok()?;
    history::history(&inputs, &[]).into_iter().find(|e| e.id == id)
}

// Verlauf: every session the app keeps, newest first, as the panel shows them now
#[tauri::command]
async fn history() -> Result<Vec<history::Entry>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let inputs = inputs::load_inputs()?;
        let items = LAST.lock().expect("last").as_ref().map(|s| s.items.clone()).unwrap_or_default();
        Ok(history::history(&inputs, &items))
    })
    .await
    .map_err(|e| e.to_string())?
}

// a command marked as known is listed but not counted, until the next
// reminder step of the flag it has now
#[tauri::command]
async fn mark_command(app: AppHandle, key: String, known: bool) -> Result<(), String> {
    let is_key = key.len() > 37 && key.as_bytes()[36] == b'/' && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/');
    if !is_key {
        return Err("key".into());
    }
    {
        let flag = LAST.lock().expect("last").as_ref().and_then(|s| s.checks.iter().find(|c| c.key == key)).and_then(|c| c.flag);
        let mut store = STORE.lock().expect("store");
        if known {
            store.known_at.insert(key, Known { flag, at: now_ms() });
        } else {
            store.known_at.remove(&key);
        }
        save(&store)?;
    }
    tauri::async_runtime::spawn_blocking(move || refresh(&app)).await.map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    settings: Settings,
    problem: Option<String>,
}

#[tauri::command]
fn get_settings() -> SettingsView {
    let (settings, problem) = SETTINGS.lock().expect("settings").clone();
    SettingsView { settings, problem }
}

// checked, saved and in force at once; what lies around counts as reminded of
// under the new steps
#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let settings = settings.checked()?;
    settings::save(&settings)?;
    panel::set_appearance(&app, settings.appearance);
    {
        let mut store = STORE.lock().expect("store");
        notify::rebase(&mut store, &settings, now_ms());
        save(&store)?;
    }
    *SETTINGS.lock().expect("settings") = (settings.clone(), None);
    Ok(settings)
}

#[tauri::command]
fn set_pinned(pinned: bool) -> bool {
    panel::PINNED.store(pinned, Ordering::SeqCst);
    pinned
}

#[tauri::command]
fn close_panel(app: AppHandle) {
    panel::hide(&app);
}

// the size of the content: small, only as wide as the counts
#[tauri::command]
fn panel_size(app: AppHandle, width: Option<f64>, height: f64) {
    panel::set_size(&app, width, height);
}

// small, the panel stays open like a pinned one
#[tauri::command]
fn set_compact(compact: bool) {
    panel::COMPACT.store(compact, Ordering::SeqCst);
}

// whether the panel was moved to a place of its own, and back below the item
#[tauri::command]
fn is_placed() -> bool {
    panel::is_placed()
}

#[tauri::command]
fn reset_place(app: AppHandle) {
    panel::reset_place(&app);
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![snapshot, mark, open_chat, save_case, set_pinned, set_compact, close_panel, panel_size, get_settings, set_settings, mark_command, history, is_placed, reset_place])
        .setup(|app| {
            // a menu bar app: no Dock icon, no app menu
            app.set_activation_policy(ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            if let Err(error) = handle.notification().request_permission() {
                eprintln!("notify: {error}");
            }
            // a click on a notification about one session opens its chat in
            // Claude, on any other the panel, big
            let clicked = handle.clone();
            handle.notification().on_action(move |action| {
                if action.action_id() != "tap" {
                    return;
                }
                match action.notification().and_then(|n| n.extra().get(notify::SESSION)).and_then(|v| v.as_str()) {
                    Some(id) => {
                        if let Err(error) = open_chat(clicked.clone(), id.to_string()) {
                            eprintln!("notify: {error}");
                        }
                    }
                    None => panel::open_big(&clicked),
                }
            })?;
            panel::set_appearance(&handle, SETTINGS.lock().expect("settings").0.appearance);
            tray::build(&handle)?;
            watch::start(handle.clone(), refresh)?;
            std::thread::spawn(move || refresh(&handle));
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::Focused(false) if window.label() == panel::LABEL => panel::on_blur(window.app_handle()),
            WindowEvent::Moved(at) if window.label() == panel::LABEL => panel::on_moved(window.app_handle(), *at),
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("Pulse could not start");
}
