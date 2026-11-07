// The Windows tray holds a square icon, too small for the strip of counts the
// Mac's menu bar shows (tray.rs); here that strip sits on the taskbar, beside
// the tray. Windows 11 dropped the taskbar toolbars (desk bands) that once
// showed live numbers there, so the strip is a small window of its own above
// the taskbar: topmost, never active, see-through around the strip, on a
// thread of its own that waits on nothing of Pulse's. It is not made a child
// of the taskbar: a window tied to another process's window shares its input,
// and so its hangs. It keeps clear of the taskbar's buttons (Windows 11 draws
// them in XAML, where the old taskbar windows no longer say, so UI Automation
// tells where they are), steps back when the taskbar does (slid away, or under
// a full-screen window), and a click on it opens the panel as the tray icon does.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, PhysicalPosition, PhysicalSize, Rect};
use windows::core::Interface;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationElementArray, TreeScope_Children, UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId};
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, GetSysColor, MapWindowPoints, MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, COLOR_WINDOWTEXT, DIB_RGB_COLORS, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HCF_HIGHCONTRASTON, HIGHCONTRASTW, HWINEVENTHOOK};
use windows_sys::Win32::UI::Controls::{InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, TOOLTIPS_CLASSW, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowExW, FindWindowW, GetClassNameW, GetClientRect, GetForegroundWindow, GetMessageW, GetWindow, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SendMessageW,
    SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, UpdateLayeredWindow, CHILDID_SELF, CW_USEDEFAULT, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, GW_HWNDPREV, HWND_TOPMOST, IDC_HAND, MA_NOACTIVATE, MSG, OBJID_WINDOW, SPI_GETHIGHCONTRAST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_SETTINGCHANGE, WM_SYSCOLORCHANGE, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::items::Counts;
use crate::panel;
use crate::tray::{self, Chip};

// at 100%, in pixels: the strip's height (the menu bar's), the room around it
// that is part of the window, the gap to the tray and to the buttons, and room
// for one button more, so that a new one does not land under the strip before
// the buttons are looked at again
const STRIP: f64 = 18.0;
const PAD: f64 = 6.0;
const MARGIN: f64 = 8.0;
const SLACK: f64 = 44.0;
// the digits in the taskbar's own text colour
const INK_ON_DARK: [u8; 3] = [0xff, 0xff, 0xff];
const INK_ON_LIGHT: [u8; 3] = [0x1c, 0x1c, 0x1c];

// look again: new counts, switched on or off, the taskbar moved or the
// foreground changed
const WM_LOOK: u32 = WM_APP + 1;
// timers: a look twice a second (the slow way the taskbar's changes are seen,
// such as the taskbar raised over the strip), and a look with the buttons
// once the taskbar has stopped moving, or the settings or screens changed
const TICK: usize = 1;
const SOON: usize = 2;
// the buttons are also looked at this often: a new app's button moves the
// others without moving any window
const BUTTONS_EVERY: Duration = Duration::from_secs(5);

// what the other threads tell this one
struct Shared {
    on: bool,
    // what runs shows too (settings.taskbar_running)
    running: bool,
    counts: Counts,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared { on: false, running: false, counts: Counts::default() }));
// the strip's window, once made; the taskbar and the tray as last found, for
// the hook that hears them move; the message Explorer sends when it starts anew
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static TASKBAR: AtomicIsize = AtomicIsize::new(0);
static TRAY: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
// a look already asked for and not yet taken
static LOOK_ASKED: AtomicBool = AtomicBool::new(false);
// the colours changed: read at the next look
static RETHEME: AtomicBool = AtomicBool::new(false);

pub fn start(app: AppHandle, on: bool, running: bool) {
    set_on(on, running);
    if let Err(error) = std::thread::Builder::new().name("taskbar".into()).spawn(move || run(app)) {
        eprintln!("taskbar: {error}");
    }
}

// new counts
pub fn show(counts: &Counts) {
    SHARED.lock().expect("taskbar").counts = counts.clone();
    ask_look();
}

pub fn set_on(on: bool, running: bool) {
    {
        let mut shared = SHARED.lock().expect("taskbar");
        shared.on = on;
        shared.running = running;
    }
    ask_look();
}

fn ask_look() {
    let window = WINDOW.load(Ordering::SeqCst);
    if window != 0 && !LOOK_ASKED.swap(true, Ordering::SeqCst) {
        unsafe { PostMessageW(window as HWND, WM_LOOK, 0, 0) };
    }
}

// what is drawn: the counts shown, at the taskbar's scale and size, in its
// colours
#[derive(Clone, Copy, PartialEq)]
struct Key {
    shown: [usize; 5],
    dpi: u32,
    ink: [u8; 3],
    room: (i32, i32),
}

struct State {
    app: AppHandle,
    window: HWND,
    tip: HWND,
    tip_text: String,
    // the taskbar as last found, and the hook on its thread that hears it move
    taskbar: HWND,
    moved: HWINEVENTHOOK,
    ink: [u8; 3],
    uia: Option<IUIAutomation>,
    // Windows 11's frame of buttons, once found; none: Windows 10's taskbar,
    // or Windows 11's not drawn yet, and it is looked for again
    frame: Option<IUIAutomationElement>,
    // where the buttons are, and what sits by the tray on its side of them
    busy: Vec<RECT>,
    by_tray: Option<RECT>,
    buttons_at: Option<Instant>,
    failed: Option<String>,
    drawn: Option<Key>,
    // the last picture did not reach the window (said once)
    blank: bool,
    // where the strip is; None: hidden
    placed: Option<(i32, i32)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

// the taskbar and the tray as they are, when the strip may sit on them
struct Bar {
    taskbar: HWND,
    rect: RECT,
    tray: RECT,
    dpi: u32,
}

fn run(app: AppHandle) {
    // pixels on every screen, whatever the process was told
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let controls = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    unsafe { InitCommonControlsEx(&controls) };
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = w!("PulseTaskbarCounts");
    let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance, lpszClassName: class, hCursor: unsafe { LoadCursorW(null_mut(), IDC_HAND) }, ..Default::default() };
    unsafe { RegisterClassW(&wc) };
    let window = unsafe { CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class, w!("Pulse"), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), instance, null()) };
    if window.is_null() {
        eprintln!("taskbar: no window");
        return;
    }
    let tip = unsafe { CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, TOOLTIPS_CLASSW, null(), WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, window, null_mut(), instance, null()) };
    let mut text: Vec<u16> = "Pulse".encode_utf16().chain(Some(0)).collect();
    let mut info = tool(window);
    info.lpszText = text.as_mut_ptr();
    // SAFETY: the tool and its text outlive the call, which copies them
    unsafe { SendMessageW(tip, TTM_ADDTOOLW, 0, &info as *const TTTOOLINFOW as LPARAM) };
    TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }, Ordering::SeqCst);
    STATE.set(Some(State {
        app,
        window,
        tip,
        tip_text: "Pulse".to_string(),
        taskbar: null_mut(),
        moved: null_mut(),
        ink: ink(),
        uia: None,
        frame: None,
        busy: Vec::new(),
        by_tray: None,
        buttons_at: None,
        failed: None,
        drawn: None,
        blank: false,
        placed: None,
    }));
    WINDOW.store(window as isize, Ordering::SeqCst);
    unsafe { SetTimer(window, TICK, 500, None) };
    // another window in front: maybe one that fills the screen, or the
    // taskbar, clicked, raised above the strip
    unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) };
    with_state(|s| update(s, true));
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// the state, unless a look is under way on this thread already (a message it
// sent came back): then another look after it
fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut state) => {
            if let Some(state) = state.as_mut() {
                f(state)
            }
        }
        Err(_) => ask_look(),
    })
}

unsafe extern "system" fn window_proc(window: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_LBUTTONUP => {
            with_state(|s| open_panel(s));
            return 0;
        }
        WM_LOOK => {
            LOOK_ASKED.store(false, Ordering::SeqCst);
            with_state(|s| update(s, false));
            return 0;
        }
        WM_TIMER => {
            if wparam == SOON {
                unsafe { KillTimer(window, SOON) };
            }
            with_state(|s| update(s, wparam == SOON));
            return 0;
        }
        // the colours switched, the taskbar set otherwise, screens changed:
        // a look soon, from the queue, as whoever sent this waits for the
        // answer, and a look asks Explorer about its buttons
        WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if msg == WM_SETTINGCHANGE || msg == WM_SYSCOLORCHANGE {
                RETHEME.store(true, Ordering::SeqCst);
            }
            unsafe { SetTimer(window, SOON, 10, None) };
            if msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
                return 0;
            }
        }
        // Explorer started anew: a new taskbar
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            unsafe { SetTimer(window, SOON, 10, None) };
        }
        _ => {}
    }
    unsafe { DefWindowProcW(window, msg, wparam, lparam) }
}

unsafe extern "system" fn on_event(_hook: HWINEVENTHOOK, event: u32, hwnd: HWND, object: i32, child: i32, _thread: u32, _time: u32) {
    if event == EVENT_OBJECT_LOCATIONCHANGE {
        if object != OBJID_WINDOW || child != CHILDID_SELF as i32 {
            return;
        }
        let moved = hwnd as isize;
        if moved != TASKBAR.load(Ordering::SeqCst) && moved != TRAY.load(Ordering::SeqCst) {
            return;
        }
        // the buttons again once it settles
        unsafe { SetTimer(WINDOW.load(Ordering::SeqCst) as HWND, SOON, 300, None) };
    }
    ask_look();
}

// the strip as it should be now: drawn anew if the counts, the scale or the
// colours changed, put by the tray, or hidden. The buttons are looked at when
// `buttons` says so, or when they were last looked at a while ago, as by a
// strip shown again after a time hidden
fn update(s: &mut State, buttons: bool) {
    if RETHEME.swap(false, Ordering::SeqCst) {
        s.ink = ink();
    }
    let (on, running, counts) = {
        let shared = SHARED.lock().expect("taskbar");
        (shared.on, shared.running, shared.counts.clone())
    };
    // the tray's tooltip, and what runs named in it when it shows here
    let tip = tray::tooltip(&counts, running);
    if tip != s.tip_text {
        set_tip(s, tip);
    }
    if !on || tray::chips(&counts, running).is_empty() {
        return hide(s);
    }
    let Some(bar) = find_bar(s) else { return hide(s) };
    if buttons || !s.buttons_at.is_some_and(|at| at.elapsed() < BUTTONS_EVERY) {
        look_at_buttons(s, bar.taskbar);
    }
    let scale = bar.dpi as f64 / 96.0;
    let px = |at_100: f64| (at_100 * scale).round() as i32;
    let room = (bar.rect.right - bar.rect.left, bar.rect.bottom - bar.rect.top);
    let end = s.by_tray.map_or(bar.tray, |by| union(bar.tray, by));
    let at = |chips: &[Chip]| place(bar.rect, end, &s.busy, layout(tray::width(chips), scale, room).size(), px(MARGIN), px(SLACK));
    let Some((chips, runs, (x, y))) = fitting(&counts, running, at) else { return hide(s) };
    let key = Key { shown: [counts.blocked, counts.waiting, counts.review, counts.commands, runs], dpi: bar.dpi, ink: s.ink, room };
    if s.drawn != Some(key) {
        if !upload(s.window, &picture(&chips, scale, s.ink, room), x, y) {
            // drawn again at the next look
            s.drawn = None;
            if !s.blank {
                eprintln!("taskbar: the strip could not be drawn");
                s.blank = true;
            }
            return hide(s);
        }
        s.blank = false;
        s.drawn = Some(key);
    } else if s.placed != Some((x, y)) {
        unsafe { SetWindowPos(s.window, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE) };
    }
    if s.placed.is_none() {
        unsafe {
            ShowWindow(s.window, SW_SHOWNOACTIVATE);
            SetWindowPos(s.window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    } else {
        raise(s);
    }
    s.placed = Some((x, y));
}

// the chips by the tray and where they go: all of them, or when only the open
// states have room, those alone, as what runs needs the person least; with
// how many run among them. None: not even those have room
fn fitting(counts: &Counts, running: bool, at: impl Fn(&[Chip]) -> Option<(i32, i32)>) -> Option<(Vec<Chip>, usize, (i32, i32))> {
    let all = tray::chips(counts, running);
    let runs = if running { counts.running } else { 0 };
    match at(&all) {
        Some(xy) if !all.is_empty() => Some((all, runs, xy)),
        None if runs > 0 => {
            let open = tray::chips(counts, false);
            at(&open).filter(|_| !open.is_empty()).map(|xy| (open, 0, xy))
        }
        _ => None,
    }
}

fn hide(s: &mut State) {
    if s.placed.take().is_some() {
        unsafe { ShowWindow(s.window, SW_HIDE) };
    }
}

// a click on the taskbar brings it to the top of the topmost windows, over
// the strip: the strip goes back above it
fn raise(s: &State) {
    let mut above = unsafe { GetWindow(s.window, GW_HWNDPREV) };
    while !above.is_null() {
        if above == s.taskbar {
            unsafe { SetWindowPos(s.window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
            return;
        }
        above = unsafe { GetWindow(above, GW_HWNDPREV) };
    }
}

// the taskbar with the tray, when the strip may sit on it: shown, all on its
// screen (an auto-hidden one slides off it) and not under a window that fills
// that screen
fn find_bar(s: &mut State) -> Option<Bar> {
    let taskbar = unsafe { FindWindowW(w!("Shell_TrayWnd"), null()) };
    if taskbar.is_null() {
        return None;
    }
    if taskbar != s.taskbar {
        bind(s, taskbar);
    }
    let tray = unsafe { FindWindowExW(taskbar, null_mut(), w!("TrayNotifyWnd"), null()) };
    TRAY.store(tray as isize, Ordering::SeqCst);
    if tray.is_null() || unsafe { IsWindowVisible(taskbar) } == 0 {
        return None;
    }
    let (mut rect, mut tray_rect): (RECT, RECT) = Default::default();
    if unsafe { GetWindowRect(taskbar, &mut rect) } == 0 || unsafe { GetWindowRect(tray, &mut tray_rect) } == 0 {
        return None;
    }
    let monitor = unsafe { MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `info` is a MONITORINFO of the size it says
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 || !within(&rect, &info.rcMonitor) || full_screen(monitor, &info.rcMonitor) {
        return None;
    }
    Some(Bar { taskbar, rect, tray: tray_rect, dpi: unsafe { GetDpiForWindow(taskbar) }.max(96) })
}

// a new taskbar (at the start, or Explorer started anew): the hook that hears
// it move goes to its thread, and its buttons are looked for anew
fn bind(s: &mut State, taskbar: HWND) {
    if !s.moved.is_null() {
        unsafe { UnhookWinEvent(s.moved) };
    }
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(taskbar, &mut process) };
    s.moved = unsafe { SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, null_mut(), Some(on_event), process, thread, WINEVENT_OUTOFCONTEXT) };
    s.taskbar = taskbar;
    TASKBAR.store(taskbar as isize, Ordering::SeqCst);
    s.frame = None;
    s.buttons_at = None;
}

// the foreground window fills the taskbar's screen: a video, a game, a slide
// show; the taskbar steps back for it while it is in front
fn full_screen(monitor: *mut c_void, screen: &RECT) -> bool {
    let front = unsafe { GetForegroundWindow() };
    if front.is_null() || unsafe { MonitorFromWindow(front, MONITOR_DEFAULTTONEAREST) } != monitor {
        return false;
    }
    let mut class = [0u16; 32];
    // SAFETY: the buffer is as long as it says
    let n = unsafe { GetClassNameW(front, class.as_mut_ptr(), class.len() as i32) }.max(0) as usize;
    let mut inside = RECT::default();
    if unsafe { GetClientRect(front, &mut inside) } == 0 {
        return false;
    }
    // SAFETY: a RECT is the two POINTs the call maps, the screen's way round
    // for a window that reads right to left too
    unsafe { MapWindowPoints(front, null_mut(), &mut inside as *mut RECT as *mut POINT, 2) };
    fills(&String::from_utf16_lossy(&class[..n]), &inside, screen)
}

// a window fills the screen when what it draws in (its client area) covers
// it: a maximized one keeps its title bar out, and an auto-hidden taskbar
// still slides up over it. The desktop is as large, and is no full-screen
// window
fn fills(class: &str, inside: &RECT, screen: &RECT) -> bool {
    !["Progman", "WorkerW", "Shell_TrayWnd"].contains(&class) && within(screen, inside)
}

// the taskbar's text colour: a contrast theme's own, else white, or near black
// when Windows is set light
fn ink() -> [u8; 3] {
    let mut contrast = HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    // SAFETY: `contrast` is a HIGHCONTRASTW of the size it says
    let read = unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, contrast.cbSize, &mut contrast as *mut HIGHCONTRASTW as *mut c_void, 0) } != 0;
    if read && contrast.dwFlags & HCF_HIGHCONTRASTON != 0 {
        let [r, g, b, _] = unsafe { GetSysColor(COLOR_WINDOWTEXT) }.to_le_bytes();
        return [r, g, b];
    }
    let (mut value, mut size) = (0u32, size_of::<u32>() as u32);
    // SAFETY: the value goes to a u32, `size` bytes
    let read = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("SystemUsesLightTheme"), RRF_RT_REG_DWORD, null_mut(), &mut value as *mut u32 as *mut c_void, &mut size) };
    if read == 0 && value == 1 {
        INK_ON_LIGHT
    } else {
        INK_ON_DARK
    }
}

fn look_at_buttons(s: &mut State, taskbar: HWND) {
    s.buttons_at = Some(Instant::now());
    match buttons(s, taskbar) {
        Ok((busy, by_tray)) => {
            s.busy = busy;
            s.by_tray = by_tray;
            s.failed = None;
        }
        // what was found before stands; the frame is looked for anew
        Err(error) => {
            s.frame = None;
            let error = error.to_string();
            if s.failed.as_ref() != Some(&error) {
                eprintln!("taskbar: {error}");
                s.failed = Some(error);
            }
        }
    }
}

// where the taskbar's buttons are: on Windows 11 all that its frame holds
// (widgets, Start, search, the apps), on Windows 10 the apps and the toolbars
// beside them. What sits by the tray (the widgets when the buttons are on the
// left, the toolbars) is kept apart: the strip goes on its far side
fn buttons(s: &mut State, taskbar: HWND) -> windows::core::Result<(Vec<RECT>, Option<RECT>)> {
    let uia = match &s.uia {
        Some(uia) => uia.clone(),
        None => {
            // the object that takes timeouts: a busy Explorer costs a second
            // at most
            let uia: IUIAutomation = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
            let timed = uia.cast::<IUIAutomation2>().and_then(|uia| unsafe {
                uia.SetConnectionTimeout(1000)?;
                uia.SetTransactionTimeout(1000)
            });
            if let Err(error) = timed {
                eprintln!("taskbar: no timeouts: {error}");
            }
            s.uia = Some(uia.clone());
            uia
        }
    };
    if s.frame.is_none() {
        s.frame = find_frame(&uia, taskbar)?;
    }
    let cache = unsafe { uia.CreateCacheRequest() }?;
    unsafe {
        cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
        cache.AddProperty(UIA_AutomationIdPropertyId)?;
    }
    let all = unsafe { uia.CreateTrueCondition() }?;
    let mut found = Vec::new();
    if let Some(frame) = &s.frame {
        add(&mut found, unsafe { frame.FindAllBuildCache(TreeScope_Children, &all, &cache) }?)?;
    } else {
        // Windows 10's: the bands of the taskbar's rebar, but for the one of
        // the apps, whose buttons are in its list
        let rebar = unsafe { FindWindowExW(taskbar, null_mut(), w!("ReBarWindow32"), null()) };
        if !rebar.is_null() {
            let tasks = unsafe { FindWindowExW(rebar, null_mut(), w!("MSTaskSwWClass"), null()) };
            let list = unsafe { FindWindowExW(tasks, null_mut(), w!("MSTaskListWClass"), null()) };
            let mut band = unsafe { FindWindowExW(rebar, null_mut(), null(), null()) };
            while !band.is_null() {
                let mut r = RECT::default();
                if band != tasks && unsafe { IsWindowVisible(band) } != 0 && unsafe { GetWindowRect(band, &mut r) } != 0 {
                    found.push((r, true));
                }
                band = unsafe { FindWindowExW(rebar, band, null(), null()) };
            }
            if !list.is_null() {
                let element = unsafe { uia.ElementFromHandle(windows::Win32::Foundation::HWND(list)) }?;
                add(&mut found, unsafe { element.FindAllBuildCache(TreeScope_Children, &all, &cache) }?)?;
            }
        }
    }
    let (mut rect, mut tray) = (RECT::default(), RECT::default());
    unsafe {
        GetWindowRect(taskbar, &mut rect);
        GetWindowRect(FindWindowExW(taskbar, null_mut(), w!("TrayNotifyWnd"), null()), &mut tray);
    }
    Ok(sort_out(rect, tray, found))
}

// each element's rect, and whether it is the widgets
fn add(found: &mut Vec<(RECT, bool)>, kids: IUIAutomationElementArray) -> windows::core::Result<()> {
    for i in 0..unsafe { kids.Length() }? {
        let kid = unsafe { kids.GetElement(i) }?;
        let r = unsafe { kid.CachedBoundingRectangle() }?;
        let widgets = unsafe { kid.CachedAutomationId() }.is_ok_and(|id| id == "WidgetsButton");
        found.push((RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom }, widgets));
    }
    Ok(())
}

// Windows 11's taskbar holds its buttons in one XAML frame, two levels down.
// None: there is none (Windows 10, or not drawn yet); an error ends the walk,
// so that what was found before stands
fn find_frame(uia: &IUIAutomation, taskbar: HWND) -> windows::core::Result<Option<IUIAutomationElement>> {
    let walker = unsafe { uia.ControlViewWalker() }?;
    let mut level = vec![unsafe { uia.ElementFromHandle(windows::Win32::Foundation::HWND(taskbar)) }?];
    for _ in 0..2 {
        let mut below = Vec::new();
        for element in &level {
            let mut child = walked(unsafe { walker.GetFirstChildElement(element) })?;
            while let Some(kid) = child {
                if unsafe { kid.CurrentAutomationId() }? == "TaskbarFrame" {
                    return Ok(Some(kid));
                }
                child = walked(unsafe { walker.GetNextSiblingElement(&kid) })?;
                below.push(kid);
            }
        }
        level = below;
    }
    Ok(None)
}

// a walk's next element; None at the end, where UI Automation gives a null
// element, which the bindings make an error without a code
fn walked(next: windows::core::Result<IUIAutomationElement>) -> windows::core::Result<Option<IUIAutomationElement>> {
    match next {
        Ok(element) => Ok(Some(element)),
        Err(error) if error.code().is_ok() => Ok(None),
        Err(error) => Err(error),
    }
}

// the buttons that take room, and what sits on the tray's half of the taskbar
// and is no app's (the widgets, Windows 10's toolbars), taken together; empty
// ones (hidden) take none
fn sort_out(bar: RECT, tray: RECT, found: Vec<(RECT, bool)>) -> (Vec<RECT>, Option<RECT>) {
    let across = bar.right - bar.left >= bar.bottom - bar.top;
    let middle = |r: &RECT| if across { r.left + r.right } else { r.top + r.bottom };
    let half = middle(&bar);
    let tray_after = middle(&tray) >= half;
    let mut busy = Vec::new();
    let mut by_tray = None;
    for (r, beside) in found {
        if r.right <= r.left || r.bottom <= r.top {
            continue;
        }
        if beside && (middle(&r) >= half) == tray_after {
            by_tray = Some(by_tray.map_or(r, |by| union(by, r)));
        } else {
            busy.push(r);
        }
    }
    (busy, by_tray)
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}

// `inner` lies all within `outer`
fn within(inner: &RECT, outer: &RECT) -> bool {
    inner.left >= outer.left && inner.top >= outer.top && inner.right <= outer.right && inner.bottom <= outer.bottom
}

// where the strip goes: on the taskbar, by the tray (`end`) on its inner side
// (left of it, or right of it where the screen reads right to left, or above it
// on a taskbar at a side), clear of the buttons with room for one more; None
// when there is no such room
fn place(bar: RECT, end: RECT, busy: &[RECT], (w, h): (i32, i32), margin: i32, slack: i32) -> Option<(i32, i32)> {
    if bar.right - bar.left >= bar.bottom - bar.top {
        if h > bar.bottom - bar.top {
            return None;
        }
        let y = bar.top + (bar.bottom - bar.top - h) / 2;
        if end.left + end.right >= bar.left + bar.right {
            let x = end.left - margin - w;
            let taken = busy.iter().filter(|b| b.left < end.left).map(|b| b.right).max().unwrap_or(bar.left);
            (x - margin - slack >= taken).then_some((x, y))
        } else {
            let x = end.right + margin;
            let taken = busy.iter().filter(|b| b.right > end.right).map(|b| b.left).min().unwrap_or(bar.right);
            (x + w + margin + slack <= taken).then_some((x, y))
        }
    } else {
        if w > bar.right - bar.left {
            return None;
        }
        let x = bar.left + (bar.right - bar.left - w) / 2;
        let y = end.top - margin - h;
        let taken = busy.iter().filter(|b| b.top < end.top).map(|b| b.bottom).max().unwrap_or(bar.top);
        (y - margin - slack >= taken).then_some((x, y))
    }
}

// the window's image: premultiplied BGRA, top row first
struct Picture {
    bgra: Vec<u8>,
    w: i32,
    h: i32,
}

// a strip `width` wide (tray::HEIGHT high) at the taskbar's scale: its size,
// and the room around it that a click finds too. Across a taskbar (`room`: its
// width and height) too thin for strip and room, the room is less
struct Layout {
    w: usize,
    h: usize,
    pad_x: usize,
    pad_y: usize,
}

impl Layout {
    // the window's size
    fn size(&self) -> (i32, i32) {
        ((self.w + 2 * self.pad_x) as i32, (self.h + 2 * self.pad_y) as i32)
    }
}

fn layout(width: usize, scale: f64, (room_w, room_h): (i32, i32)) -> Layout {
    let h = ((STRIP * scale).round() as usize).max(1);
    let w = ((width * h + tray::HEIGHT / 2) / tray::HEIGHT).max(1);
    let pad = (PAD * scale).round() as usize;
    let fit = |room: i32, size: usize| pad.min((room.max(0) as usize).saturating_sub(size) / 2);
    Layout { w, h, pad_x: fit(room_w, w), pad_y: fit(room_h, h) }
}

// the strip as laid out (layout), the room around it nearly clear, as a clear
// pixel would let the click through
fn picture(chips: &[Chip], scale: f64, ink: [u8; 3], room: (i32, i32)) -> Picture {
    let (rgba, width) = tray::strip(chips, Some(ink));
    let Layout { w, h, pad_x, pad_y } = layout(width, scale, room);
    let small = shrink(&rgba, width, tray::HEIGHT, w, h);
    let (pw, ph) = (w + 2 * pad_x, h + 2 * pad_y);
    let mut bgra = [0, 0, 0, 1].repeat(pw * ph);
    for y in 0..h {
        for x in 0..w {
            let (from, to) = ((y * w + x) * 4, ((y + pad_y) * pw + x + pad_x) * 4);
            let a = small[from + 3] as u32;
            for c in 0..4 {
                bgra[to + c] = (small[from + c] as u32 + bgra[to + c] as u32 * (255 - a) / 255) as u8;
            }
        }
    }
    Picture { bgra, w: pw as i32, h: ph as i32 }
}

// straight RGBA, w by h, to premultiplied BGRA, to_w by to_h: each new pixel
// the mean of the part of the old ones it covers
fn shrink(rgba: &[u8], w: usize, h: usize, to_w: usize, to_h: usize) -> Vec<u8> {
    let (fx, fy) = (w as f64 / to_w as f64, h as f64 / to_h as f64);
    let mut out = vec![0; to_w * to_h * 4];
    for oy in 0..to_h {
        let (y0, y1) = (oy as f64 * fy, (oy + 1) as f64 * fy);
        for ox in 0..to_w {
            let (x0, x1) = (ox as f64 * fx, (ox + 1) as f64 * fx);
            let mut sum = [0.0f64; 4];
            for sy in (y0.floor() as usize)..(y1.ceil() as usize).min(h) {
                let wy = y1.min(sy as f64 + 1.0) - y0.max(sy as f64);
                for sx in (x0.floor() as usize)..(x1.ceil() as usize).min(w) {
                    let wx = x1.min(sx as f64 + 1.0) - x0.max(sx as f64);
                    let i = (sy * w + sx) * 4;
                    let a = rgba[i + 3] as f64 / 255.0 * wx * wy;
                    sum[0] += rgba[i + 2] as f64 * a;
                    sum[1] += rgba[i + 1] as f64 * a;
                    sum[2] += rgba[i] as f64 * a;
                    sum[3] += 255.0 * a;
                }
            }
            let area = (x1 - x0) * (y1 - y0);
            let o = (oy * to_w + ox) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / area).round().min(255.0) as u8;
            }
        }
    }
    out
}

// the picture into the window, at (x, y); false if it did not get there
fn upload(window: HWND, picture: &Picture, x: i32, y: i32) -> bool {
    let screen = unsafe { GetDC(null_mut()) };
    let dc = unsafe { CreateCompatibleDC(screen) };
    let header = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: picture.w, biHeight: -picture.h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..Default::default() };
    let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
    let mut bits = null_mut();
    let bitmap = unsafe { CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
    let mut drawn = false;
    if !bitmap.is_null() && !bits.is_null() {
        // SAFETY: `bits` is the bitmap's w * h * 4 bytes, made for the
        // picture's w and h, as long as the picture's pixels
        unsafe { std::ptr::copy_nonoverlapping(picture.bgra.as_ptr(), bits as *mut u8, picture.bgra.len()) };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let (to, size, from) = (POINT { x, y }, SIZE { cx: picture.w, cy: picture.h }, POINT { x: 0, y: 0 });
        unsafe {
            let old = SelectObject(dc, bitmap);
            drawn = UpdateLayeredWindow(window, screen, &to, &size, dc, &from, 0, &blend, ULW_ALPHA) != 0;
            SelectObject(dc, old);
            DeleteObject(bitmap);
        }
    }
    unsafe {
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);
    }
    drawn
}

// the tooltip's one tool: the whole window. The size of the version 5 common
// controls take as well as 6
fn tool(window: HWND) -> TTTOOLINFOW {
    TTTOOLINFOW { cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32, uFlags: TTF_IDISHWND | TTF_SUBCLASS, hwnd: window, uId: window as usize, ..Default::default() }
}

fn set_tip(s: &mut State, text: String) {
    let mut wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let mut info = tool(s.window);
    info.lpszText = wide.as_mut_ptr();
    // SAFETY: the tool and its text outlive the call, which copies them
    unsafe { SendMessageW(s.tip, TTM_UPDATETIPTEXTW, 0, &info as *const TTTOOLINFOW as LPARAM) };
    s.tip_text = text;
}

// as a click on the tray icon: on the main thread, which the panel needs
fn open_panel(s: &State) {
    let mut r = RECT::default();
    if unsafe { GetWindowRect(s.window, &mut r) } == 0 {
        return;
    }
    let rect = Rect { position: PhysicalPosition::new(r.left, r.top).into(), size: PhysicalSize::new((r.right - r.left) as u32, (r.bottom - r.top) as u32).into() };
    let app = s.app.clone();
    if let Err(error) = s.app.run_on_main_thread(move || panel::toggle(&app, rect)) {
        eprintln!("taskbar: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    fn sides(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    // a 2560 by 1440 screen with its taskbar at the foot, the buttons in the
    // middle and the widgets at the start, as Windows 11 sets it
    fn bar() -> RECT {
        rect(0, 1392, 2560, 1440)
    }
    fn tray() -> RECT {
        rect(2270, 1392, 2560, 1440)
    }
    fn buttons(end: i32) -> Vec<RECT> {
        vec![rect(6, 1392, 158, 1440), rect(926, 1392, end, 1440)]
    }

    #[test]
    fn the_strip_sits_by_the_tray_clear_of_the_buttons() {
        assert_eq!(place(bar(), tray(), &buttons(1635), (120, 30), 8, 44), Some((2142, 1401)));
        // the buttons reach too near: no room for one more
        assert_eq!(place(bar(), tray(), &buttons(2100), (120, 30), 8, 44), None);
        assert_eq!(place(bar(), tray(), &buttons(2090), (120, 30), 8, 44), Some((2142, 1401)));
        // no buttons known: by the tray all the same
        assert_eq!(place(bar(), tray(), &[], (120, 30), 8, 44), Some((2142, 1401)));
        // too tall for the taskbar
        assert_eq!(place(bar(), tray(), &[], (120, 49), 8, 44), None);
    }

    #[test]
    fn the_widgets_and_the_toolbars_by_the_tray_are_passed() {
        let widgets = rect(2110, 1392, 2262, 1440);
        let found = vec![(rect(6, 1392, 50, 1440), false), (rect(50, 1392, 500, 1440), false), (widgets, true), (rect(0, 0, 0, 0), false)];
        let (busy, by_tray) = sort_out(bar(), tray(), found);
        assert_eq!(busy.len(), 2);
        let by_tray = by_tray.expect("widgets by the tray");
        assert_eq!(place(bar(), union(tray(), by_tray), &busy, (120, 30), 8, 44), Some((1982, 1401)));
        // at the start of the taskbar the widgets are a button like the others
        let (busy, by_tray) = sort_out(bar(), tray(), vec![(rect(6, 1392, 158, 1440), true)]);
        assert_eq!((busy.len(), by_tray.is_none()), (1, true));
        // Windows 10's toolbars by the tray, passed together
        let (busy, by_tray) = sort_out(bar(), tray(), vec![(rect(50, 1392, 900, 1440), false), (rect(2000, 1392, 2100, 1440), true), (rect(2100, 1392, 2262, 1440), true)]);
        assert_eq!((busy.len(), by_tray.map(sides)), (1, Some((2000, 1392, 2262, 1440))));
        // on a taskbar at a side, by the tray is below its middle
        let (busy, by_tray) = sort_out(rect(0, 0, 62, 1440), rect(0, 1200, 62, 1440), vec![(rect(0, 1100, 62, 1190), true), (rect(0, 0, 62, 60), true)]);
        assert_eq!((busy.into_iter().map(sides).collect::<Vec<_>>(), by_tray.map(sides)), (vec![(0, 0, 62, 60)], Some((0, 1100, 62, 1190))));
    }

    #[test]
    fn right_to_left_and_at_a_side_the_strip_still_sits_by_the_tray() {
        // the tray at the left where the screen reads right to left
        let tray = rect(0, 1392, 290, 1440);
        assert_eq!(place(bar(), tray, &[rect(925, 1392, 1635, 1440)], (120, 30), 8, 44), Some((298, 1401)));
        assert_eq!(place(bar(), tray, &[rect(400, 1392, 1635, 1440)], (120, 30), 8, 44), None);
        // a taskbar at the left: above the tray, if it is wide enough
        let side = rect(0, 0, 62, 1440);
        let tray = rect(0, 1200, 62, 1440);
        assert_eq!(place(side, tray, &[rect(0, 0, 62, 600)], (50, 30), 8, 44), Some((6, 1162)));
        assert_eq!(place(side, tray, &[], (100, 30), 8, 44), None);
    }

    #[test]
    fn a_taskbar_slid_away_or_a_full_screen_window_is_seen() {
        let screen = rect(0, 0, 2560, 1440);
        assert!(within(&bar(), &screen));
        // auto-hide: all but two pixels below the screen
        assert!(!within(&rect(0, 1438, 2560, 1486), &screen));
        // a video or a game: what it draws in covers the screen, whatever
        // its frame
        assert!(fills("Chrome_WidgetWin_1", &screen, &screen));
        assert!(fills("UnityWndClass", &rect(-8, -8, 2568, 1448), &screen));
        // a maximized window keeps its title bar out: under an auto-hidden
        // taskbar, which slides up over it, and beside one that is shown
        assert!(!fills("Notepad", &rect(0, 31, 2560, 1440), &screen));
        assert!(!fills("Notepad", &rect(0, 31, 2560, 1392), &screen));
        // the desktop is no full-screen window
        assert!(!fills("WorkerW", &screen, &screen));
        assert!(!fills("Progman", &screen, &screen));
    }

    #[test]
    fn the_picture_is_premultiplied_and_halved_at_100() {
        // two by two white at half alpha and clear: one pixel, a quarter covered
        let rgba = [[255, 255, 255, 128], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]].concat();
        assert_eq!(shrink(&rgba, 2, 2, 1, 1), vec![32, 32, 32, 32]);
        // red, opaque: blue first
        assert_eq!(shrink(&[255, 0, 0, 255], 1, 1, 1, 1), vec![0, 0, 255, 255]);
        let counts = Counts { blocked: 2, waiting: 1, ..Counts::default() };
        let chips = tray::chips(&counts, false);
        let (_, width) = tray::strip(&chips, Some(INK_ON_DARK));
        let p = picture(&chips, 1.0, INK_ON_DARK, (2560, 48));
        assert_eq!((p.w, p.h), ((width as i32 + 1) / 2 + 12, 30));
        // the size placed is the size drawn
        assert_eq!(layout(tray::width(&chips), 1.0, (2560, 48)).size(), (p.w, p.h));
        // nearly clear around the strip, and never more colour than cover
        assert_eq!(&p.bgra[..4], &[0, 0, 0, 1]);
        assert!(p.bgra.chunks(4).all(|px| px[..3].iter().all(|&c| c <= px[3])));
        let p = picture(&chips, 1.5, INK_ON_DARK, (2560, 72));
        assert_eq!(p.h, 27 + 18);
        // Windows 10's small buttons at 125%: a taskbar 38 high, the strip 23
        // and less room above and below it, so that it fits
        let p = picture(&chips, 1.25, INK_ON_DARK, (2560, 38));
        assert_eq!((p.h, p.w - (width as i32 * 23 + 18) / 36), (37, 16));
    }

    #[test]
    fn without_room_for_what_runs_the_open_states_stay() {
        let counts = Counts { waiting: 1, review: 3, running: 1, ..Counts::default() };
        let size = |chips: &[Chip]| layout(tray::width(chips), 1.0, (2560, 48)).size();
        let by = |busy: Vec<RECT>| move |chips: &[Chip]| place(bar(), tray(), &busy, size(chips), 8, 44);
        let shown = |found: Option<(Vec<Chip>, usize, (i32, i32))>| found.map(|(chips, runs, at)| (chips.len(), runs, at));
        // the buttons up to where the open states just fit
        let open = size(&tray::chips(&counts, false)).0;
        let near = 2270 - 2 * 8 - 44 - open;
        assert_eq!(shown(fitting(&counts, true, by(buttons(near)))), Some((2, 0, (2262 - open, 1401))));
        // with room for all, what runs shows too, last
        let all = size(&tray::chips(&counts, true)).0;
        assert_eq!(shown(fitting(&counts, true, by(buttons(near - (all - open))))), Some((3, 1, (2262 - all, 1401))));
        // not asked for: the open states alone, as before
        assert_eq!(shown(fitting(&counts, false, by(buttons(near)))), Some((2, 0, (2262 - open, 1401))));
        // what runs alone: with room, a strip of it; without, none, though
        // there is room for a strip of nothing
        let alone = Counts { running: 2, ..Counts::default() };
        assert_eq!(shown(fitting(&alone, true, by(Vec::new()))), Some((1, 2, (2262 - size(&tray::chips(&alone, true)).0, 1401))));
        assert_eq!(shown(fitting(&alone, true, by(buttons(2270 - 2 * 8 - 44 - size(&[]).0)))), None);
        assert_eq!(shown(fitting(&Counts::default(), true, by(Vec::new()))), None);
    }

    #[test]
    fn the_digits_take_the_taskbar_colour_and_the_symbols_keep_theirs() {
        let chips = tray::chips(&Counts { blocked: 2, review: 13, running: 4, ..Counts::default() }, true);
        let (own, _) = tray::strip(&chips, None);
        let (inked, _) = tray::strip(&chips, Some([1, 2, 3]));
        let drawn = || own.chunks(4).zip(inked.chunks(4)).filter(|(_, px)| px[3] > 0);
        assert!(drawn().any(|(a, b)| a != b));
        assert!(drawn().filter(|(a, b)| a != b).all(|(_, px)| px[..3] == [1, 2, 3]));
        assert!(drawn().any(|(a, b)| a == b));
    }

    // the taskbar of the machine the test runs on: UI Automation takes its
    // timeouts, the walk ends without an error, and on Windows 11 the frame
    // holds the buttons
    #[test]
    #[ignore]
    fn the_live_taskbar_is_read() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let uia: IUIAutomation = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }.expect("UI Automation");
        assert!(uia.cast::<IUIAutomation2>().is_ok());
        let taskbar = unsafe { FindWindowW(w!("Shell_TrayWnd"), null()) };
        assert!(!taskbar.is_null());
        let Some(frame) = find_frame(&uia, taskbar).expect("the walk") else {
            return println!("no frame: Windows 10's taskbar");
        };
        let cache = unsafe { uia.CreateCacheRequest() }.expect("cache");
        unsafe {
            cache.AddProperty(UIA_BoundingRectanglePropertyId).expect("rect");
            cache.AddProperty(UIA_AutomationIdPropertyId).expect("id");
        }
        let all = unsafe { uia.CreateTrueCondition() }.expect("condition");
        let mut found = Vec::new();
        add(&mut found, unsafe { frame.FindAllBuildCache(TreeScope_Children, &all, &cache) }.expect("buttons")).expect("rects");
        let tray = unsafe { FindWindowExW(taskbar, null_mut(), w!("TrayNotifyWnd"), null()) };
        let (busy, by_tray) = sort_out(rect_of(taskbar), rect_of(tray), found);
        assert!(!busy.is_empty());
        println!("buttons {:?}, by the tray {:?}", busy.into_iter().map(sides).collect::<Vec<_>>(), by_tray.map(sides));
    }

    fn rect_of(window: HWND) -> RECT {
        let mut r = RECT::default();
        unsafe { GetWindowRect(window, &mut r) };
        r
    }
}
