// Pulse: a menu bar item that shows what the Claude app's sessions and
// routines are doing and what of it needs the person, with a panel below it
// and notifications. Everything is worked out here; the panel only draws.

// a tray app: no console window beside it on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod background;
mod checks;
mod codex;
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
#[cfg(target_os = "macos")]
mod procs_mac;
#[cfg(windows)]
mod procs_win;
mod rules;
mod running;
mod runs;
mod said;
mod scrub;
mod settings;
mod store;
mod subagents;
#[cfg(windows)]
mod taskbar;
mod tray;
mod unread;
mod util;
mod watch;
mod work;

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};

use chrono::Local;
#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
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

// an app id as the app writes it, local_ and a UUID, or a Codex thread's, codex_ and its UUID
fn is_app_id(id: &str) -> bool {
    id.len() == 42 && (id.starts_with("local_") || id.starts_with(codex::PREFIX)) && id[6..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-'))
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
        // what park needs of a session the panel does not show is read before
        // the store is taken: that reads all the inputs, the processes too, and
        // the panel's moves and its closing wait for the store
        let lookups = park_lookups(&kind, &ids, |id| last.as_ref().is_some_and(|s| s.items.iter().any(|i| i.id == id)));
        let entries = if lookups.is_empty() {
            HashMap::new()
        } else {
            let lookups: Vec<String> = lookups.into_iter().map(str::to_owned).collect();
            tauri::async_runtime::spawn_blocking(move || {
                let ids: Vec<&str> = lookups.iter().map(String::as_str).collect();
                history_entries(&ids)
            })
            .await
            .map_err(|e| e.to_string())?
        };
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
                    let (title, project, said) = match (item, entries.get(&id)) {
                        (Some(i), _) => (i.title.clone(), i.project.clone(), i.reason.clone()),
                        (None, Some(e)) => (e.title.clone(), e.project.clone(), e.said.clone()),
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

#[cfg(target_os = "macos")]
fn open(args: &[&str]) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").args(args).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open: {status}"))
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

// the scheme of a link, "claude" of "claude://…"; a path has none, nor is a
// drive letter one ("C:\…")
#[cfg(windows)]
fn scheme(target: &str) -> Option<&str> {
    let (scheme, _) = target.split_once(':')?;
    let is_scheme = scheme.len() > 1 && scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    is_scheme.then_some(scheme)
}

// what is said when no app on this PC takes a scheme's links
#[cfg(windows)]
fn no_app(scheme: &str) -> String {
    match scheme.to_ascii_lowercase().as_str() {
        "codex" => "Codex is not installed.".into(),
        "claude" => "Claude is not installed.".into(),
        other => format!("Nothing on this PC opens {other}: links."),
    }
}

// whether an app takes links of the target's scheme, asked of the
// associations. By its friendly name: the Store's Claude and Codex have no
// command or executable to give. Only "no association" counts as none,
// anything else is left to the shell
#[cfg(windows)]
fn handler(target: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_NO_ASSOCIATION;
    use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_IS_PROTOCOL, ASSOCSTR_FRIENDLYAPPNAME};
    let Some(scheme) = scheme(target) else { return Ok(()) };
    let (name, verb) = (wide(scheme), wide("open"));
    // no buffer: only whether there is a name, and its size
    let mut size = 0;
    let result = unsafe { AssocQueryStringW(ASSOCF_IS_PROTOCOL, ASSOCSTR_FRIENDLYAPPNAME, name.as_ptr(), verb.as_ptr(), std::ptr::null_mut(), &mut size) };
    if result == (0x8007_0000 | ERROR_NO_ASSOCIATION) as i32 {
        Err(no_app(scheme))
    } else {
        Ok(())
    }
}

// a link through the shell's handler for its scheme, with no console window.
// ShellExecute succeeds also for a scheme no app takes (Windows then shows its
// own "Get an app" prompt), so that is asked first; its error covers only
// other failures of the shell
#[cfg(windows)]
fn open(args: &[&str]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Some((file, params)) = args.split_first() else { return Err("open: nothing".into()) };
    handler(file)?;
    let (verb, file, params) = (wide("open"), wide(file), wide(&params.join(" ")));
    // above 32 is success, at or below it an error code
    let result = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) } as isize;
    if result > 32 {
        Ok(())
    } else {
        Err(format!("open: ShellExecute error {result}"))
    }
}

#[tauri::command]
fn open_chat(app: AppHandle, id: String) -> Result<(), String> {
    if !is_app_id(&id) {
        return Err("id".into());
    }
    let link = match id.strip_prefix(codex::PREFIX) {
        Some(thread) => format!("codex://threads/{thread}"),
        None => format!("claude://claude.ai/epitaxy/{id}"),
    };
    // asked while the panel is still there, to tell why nothing opens
    #[cfg(windows)]
    handler(&link)?;
    // a pinned panel stays, as does the small one
    if !panel::PINNED.load(Ordering::SeqCst) && !panel::COMPACT.load(Ordering::SeqCst) {
        panel::hide(&app);
    }
    STORE.lock().expect("store").unseen.remove(&id);
    open(&[&link])
}

// the sessions to park that the panel does not show now (archived, long quiet)
fn park_lookups<'a>(kind: &str, ids: &'a [String], is_shown: impl Fn(&str) -> bool) -> Vec<&'a str> {
    if kind != "park" {
        return Vec::new();
    }
    ids.iter().map(String::as_str).filter(|id| !is_shown(id)).collect()
}

// those sessions as the Verlauf has them, from one read of the inputs
fn history_entries(ids: &[&str]) -> HashMap<String, history::Entry> {
    let Ok(inputs) = inputs::load_inputs() else { return HashMap::new() };
    history::selected(&inputs, &[], ids).into_iter().map(|e| (e.id.clone(), e)).collect()
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
    #[cfg(windows)]
    taskbar::set_on(settings.taskbar, settings.taskbar_running);
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

// whether the panel stands at a place of its own ('Back' shows), and back below the item
#[tauri::command]
fn is_placed() -> bool {
    panel::is_placed()
}

#[tauri::command]
fn reset_place(app: AppHandle) {
    panel::reset_place(&app);
}

fn main() {
    let builder = tauri::Builder::default();
    // a second start shows the panel of the first instead of a second item in
    // the tray; a debug build (npm run dev) runs beside the installed Pulse
    // instead of handing itself over to it and ending at once
    #[cfg(all(windows, not(debug_assertions)))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| panel::open_big(app)));
    builder
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![snapshot, mark, open_chat, save_case, set_pinned, set_compact, close_panel, panel_size, get_settings, set_settings, mark_command, history, is_placed, reset_place])
        .setup(|app| {
            // a menu bar app: no Dock icon, no app menu
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            if let Err(error) = handle.notification().request_permission() {
                eprintln!("notify: {error}");
            }
            // a click on a notification about one session opens its chat in
            // Claude, on any other the panel, big. On Windows a chat that
            // does not open is told in a notification of its own, about no
            // session: no panel is open to tell it, and a release build has
            // no console
            let clicked = handle.clone();
            handle.notification().on_action(move |action| {
                if action.action_id() != "tap" {
                    return;
                }
                match action.notification().and_then(|n| n.extra().get(notify::SESSION)).and_then(|v| v.as_str()) {
                    Some(id) => {
                        if let Err(error) = open_chat(clicked.clone(), id.to_string()) {
                            #[cfg(windows)]
                            notify::send(&clicked, "The chat did not open", &error, None);
                            #[cfg(not(windows))]
                            eprintln!("notify: {error}");
                        }
                    }
                    // on the main thread, as a click on the item: the plugin
                    // calls this on a thread of the notification's own, and
                    // the panel asks the main thread for the screens
                    None => {
                        let app = clicked.clone();
                        let _ = clicked.run_on_main_thread(move || panel::open_big(&app));
                    }
                }
            })?;
            panel::set_appearance(&handle, SETTINGS.lock().expect("settings").0.appearance);
            tray::build(&handle)?;
            #[cfg(windows)]
            {
                let settings = SETTINGS.lock().expect("settings").0.clone();
                taskbar::start(handle.clone(), settings.taskbar, settings.taskbar_running);
            }
            watch::start(handle.clone(), refresh)?;
            std::thread::spawn(move || refresh(&handle));
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::Focused(false) if window.label() == panel::LABEL => panel::on_blur(window.app_handle()),
            WindowEvent::Moved(at) if window.label() == panel::LABEL => panel::on_moved(window.app_handle(), *at),
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                // the panel closed (Alt+F4) keeps the place it was moved to
                if window.label() == panel::LABEL {
                    panel::hide(window.app_handle());
                } else {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("Pulse could not start")
        // the store is written on the way out (Quit, the installer, the end of
        // the session): a pinned or small panel does not close, so the place
        // it was moved to is in memory only
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                let Ok(store) = STORE.lock() else { return };
                if let Err(error) = save(&store) {
                    eprintln!("store: {error}");
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn park_looks_up_only_the_sessions_the_panel_does_not_show() {
        let ids: Vec<String> = ["local_a", "local_b", "codex_c"].map(String::from).to_vec();
        assert_eq!(park_lookups("park", &ids, |id| id == "local_a"), ["local_b", "codex_c"]);
        // no snapshot yet: each is looked up
        assert_eq!(park_lookups("park", &ids, |_| false), ["local_a", "local_b", "codex_c"]);
        assert!(park_lookups("park", &ids, |_| true).is_empty());
        // the other marks need none
        for kind in ["seen", "done", "open", "resume", "forget"] {
            assert!(park_lookups(kind, &ids, |_| false).is_empty());
        }
    }

    // these ask the registry only; nothing is opened
    #[cfg(windows)]
    #[test]
    fn a_link_has_a_scheme_a_path_none() {
        assert_eq!(scheme("claude://claude.ai/epitaxy/local_x"), Some("claude"));
        assert_eq!(scheme("codex://threads/x"), Some("codex"));
        assert_eq!(scheme("ms-settings:display"), Some("ms-settings"));
        // a drive letter is no scheme
        assert_eq!(scheme(r"C:\Users\x\notes.txt"), None);
        assert_eq!(scheme("c:/x"), None);
        assert_eq!(scheme("notes.txt"), None);
        assert_eq!(scheme("1x://y"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_scheme_no_app_takes_is_told() {
        assert_eq!(no_app("codex"), "Codex is not installed.");
        assert_eq!(no_app("Claude"), "Claude is not installed.");
        assert_eq!(handler("pulse-no-such-scheme-x://y"), Err("Nothing on this PC opens pulse-no-such-scheme-x: links.".to_string()));
        // a path is left to the shell
        assert_eq!(handler(r"C:\Windows\notepad.exe"), Ok(()));
    }
}
