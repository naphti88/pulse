// The panel under the menu bar item: it opens below the item, closes again
// when it loses focus, unless it is pinned; pinned it stays until the item is
// clicked again or it is closed. Moved by its head, pinned or not, it opens
// where it was put, until its position is reset. Made small it shows only the
// counts and stays open, as a pinned one does; a click on the item or on a
// notification that is about no one session opens it big. Its place is that of the big
// panel; smaller or bigger it keeps the edge nearer to the side of its screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, LogicalSize, Manager, Monitor, PhysicalPosition, PhysicalSize, Rect, Theme, WebviewWindow};

use crate::settings::Appearance;
use crate::store::{save, STORE};

pub const LABEL: &str = "panel";
const WIDTH: f64 = 420.0;
// the big panel is no shorter, whatever the room below its head
const MIN_HEIGHT: f64 = 200.0;
const MAX_HEIGHT: f64 = 760.0;

pub static PINNED: AtomicBool = AtomicBool::new(false);
pub static COMPACT: AtomicBool = AtomicBool::new(false);
// the panel opened above the item (a taskbar at the screen's foot, or the item
// low in one at its side): it grows and shrinks upward, its foot stays
static ABOVE: AtomicBool = AtomicBool::new(false);
// the panel stands at the place the person gave it: show() found that place on
// a screen, or they moved it since. Else it stands by the item, whatever place
// is kept for a screen that may come back
static AT_PLACE: AtomicBool = AtomicBool::new(false);
// a click on the item takes the focus from the panel first: a panel that just
// closed that way stays closed
static CLOSED_AT: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(Default::default);
// where the panel opens below the item, the last time it was worked out
static ANCHOR: LazyLock<Mutex<Option<PhysicalPosition<i32>>>> = LazyLock::new(Default::default);
// the screen that held it, its free part and its scale as they were then
static LAYOUT: LazyLock<Mutex<Option<[i32; 9]>>> = LazyLock::new(Default::default);
// where Pulse itself last moved the panel: that move is not the person's
static EXPECTED: LazyLock<Mutex<Option<PhysicalPosition<i32>>>> = LazyLock::new(Default::default);
// the head a placed panel rose from, too tall for the room below it there: it
// opens risen again and goes back once it fits, until the person moves it or it
// is taken back
static RISEN: LazyLock<Mutex<Option<i32>>> = LazyLock::new(Default::default);

fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(LABEL)
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = window(app) {
        let _ = w.hide();
    }
    keep_place();
}

// the place it was moved to, written when it closes rather than at every step of a drag
fn keep_place() {
    let store = STORE.lock().expect("store");
    if let Err(error) = save(&store) {
        eprintln!("store: {error}");
    }
}

// a place on one of the screens, else none (a screen that is gone)
fn on_screen(w: &WebviewWindow, at: [i32; 2]) -> bool {
    on_one(&screens(&w.available_monitors().unwrap_or_default()), at)
}

// the window's corner on one of these screens, or just left of one: on Windows
// that of a panel flush with a screen's left edge is in the frame it keeps
// unseen, off every screen or on the one to the left at a seam
fn on_one(screens: &[[i32; 4]], at: [i32; 2]) -> bool {
    screens.iter().any(|&[x, y, width, height]| (x - 40..x + width - 40).contains(&at[0]) && (y..y + height - 40).contains(&at[1]))
}

fn big_width(w: &WebviewWindow) -> i32 {
    (WIDTH * w.scale_factor().unwrap_or(2.0)).round() as i32
}

// the page's width, as big_width and set_size count it: on Windows the window
// is wider by a frame it keeps unseen round the page
fn current_width(w: &WebviewWindow) -> i32 {
    w.inner_size().map_or_else(|_| big_width(w), |s| s.width as i32)
}

// which of these screens (x, y, width, height) holds the point; for a point
// above or below every screen, the nearest in its column
fn screen_at(screens: &[[i32; 4]], x: i32, y: i32) -> Option<usize> {
    let off = |s: &[i32; 4]| (s[1] - y).max(y - (s[1] + s[3] - 1)).max(0);
    (0..screens.len()).filter(|&i| (screens[i][0]..screens[i][0] + screens[i][2]).contains(&x)).min_by_key(|&i| off(&screens[i]))
}

// which of these screens holds the most of a window at `at` this big: the one
// Windows takes it to be on, which set_size goes by; none for one off them all
fn most_on(screens: &[[i32; 4]], at: PhysicalPosition<i32>, (width, height): (i32, i32)) -> Option<usize> {
    let part = |s: &[i32; 4]| {
        let across = (at.x + width).min(s[0] + s[2]) - at.x.max(s[0]);
        let down = (at.y + height).min(s[1] + s[3]) - at.y.max(s[1]);
        i64::from(across.max(0)) * i64::from(down.max(0))
    };
    (0..screens.len()).filter(|&i| part(&screens[i]) > 0).max_by_key(|&i| part(&screens[i]))
}

fn screens(monitors: &[Monitor]) -> Vec<[i32; 4]> {
    monitors.iter().map(|m| [m.position().x, m.position().y, m.size().width as i32, m.size().height as i32]).collect()
}

// the free part of each, from top to bottom
fn free_parts(monitors: &[Monitor]) -> Vec<(i32, i32)> {
    monitors.iter().map(|m| m.work_area()).map(|a| (a.position.y, a.position.y + a.size.height as i32)).collect()
}

// the screen that holds the anchor, as on_one finds it: where it is, its free
// part and its scale
fn layout(monitors: &[Monitor], anchor: PhysicalPosition<i32>) -> Option<[i32; 9]> {
    monitors
        .iter()
        .map(|m| {
            let (at, size, free) = (m.position(), m.size(), m.work_area());
            [at.x, at.y, size.width as i32, size.height as i32, free.position.x, free.position.y, free.size.width as i32, free.size.height as i32, (m.scale_factor() * 1000.0).round() as i32]
        })
        .find(|&[x, y, width, height, ..]| on_one(&[[x, y, width, height]], [anchor.x, anchor.y]))
}

fn monitor_at(w: &WebviewWindow, x: i32, y: i32) -> Option<Monitor> {
    let monitors = w.available_monitors().unwrap_or_default();
    screen_at(&screens(&monitors), x, y).and_then(|i| monitors.into_iter().nth(i))
}

// whether a panel at x, y as wide as this sits on the right half of its screen
fn right_half(screens: &[[i32; 4]], x: i32, y: i32, width: i32) -> bool {
    let center = x + width / 2;
    screen_at(screens, center, y).is_some_and(|i| center > screens[i][0] + screens[i][2] / 2)
}

fn is_right(w: &WebviewWindow, x: i32, y: i32, width: i32) -> bool {
    right_half(&screens(&w.available_monitors().unwrap_or_default()), x, y, width)
}

// where a panel at x, y as wide as old stands as wide as new: the edge nearer
// the side of its screen stays, by the side of the panel as it is, so that one
// as wide as it was stays where it is
fn resized_x(screens: &[[i32; 4]], x: i32, y: i32, old: i32, new: i32) -> i32 {
    if right_half(screens, x, y, old) { x + old - new } else { x }
}

// y, or just high enough for a page this tall, in a frame `frame` taller, to end
// above the foot of the free part of its screen, from top to bottom
fn fit((top, bottom): (i32, i32), frame: i32, y: i32, height: i32) -> i32 {
    y.min(bottom.saturating_sub(frame + height).max(top))
}

// where set_size puts a panel at `at` whose page, in a frame `frame` taller, goes
// from old to new (width, height), on a screen free from top to bottom: its edge
// stays as resized_x keeps it, and so does its head. Placed by the person, a
// panel too tall for the room below its head rises just enough when it is made
// taller or shorter, or when its foot is past the free part, and goes back there
// once it fits; `from` is that head (the y it rose from, else where it stands),
// and the place stays theirs. Else as tall as it was a panel keeps its head
fn resized(screens: &[[i32; 4]], (top, bottom): (i32, i32), from: Option<i32>, at: PhysicalPosition<i32>, frame: i32, (old, old_height): (i32, i32), (new, new_height): (i32, i32)) -> PhysicalPosition<i32> {
    let y = match from {
        Some(from) if new_height != old_height || at.y + frame + new_height > bottom => fit((top, bottom), frame, from, new_height),
        _ => at.y,
    };
    PhysicalPosition::new(resized_x(screens, at.x, at.y, old, new), y)
}

// where a placed panel opens at `at`, its window (outer) and its page as big as
// they are: as set_size has it, risen just enough when not even the shortest
// big panel (least) fits below its head, on the free part of a screen (one per
// screen). That is the screen most of it is on there, which set_size goes by;
// closed risen from this place (`risen`, the head it rose from), it opens risen
// again, by the screen of its head, where it rose. Else set_size makes it as
// tall as the room there
fn opened(screens: &[[i32; 4]], free: &[(i32, i32)], risen: Option<i32>, at: PhysicalPosition<i32>, outer: (i32, i32), (width, height): (i32, i32), least: i32) -> PhysicalPosition<i32> {
    let head = screen_at(screens, at.x + width / 2, at.y);
    let by = if risen == Some(at.y) { head } else { most_on(screens, at, outer).or(head) };
    let free = by.and_then(|i| free.get(i));
    PhysicalPosition::new(at.x, free.map_or(at.y, |&free| fit(free, outer.1 - height, at.y, height.min(least))))
}

// how tall the page of a placed panel opened at `at` is made before its first
// measure, its window (outer) and its page as tall as they are: no taller than
// the room below its head on the free part of the screen most of it is on, as
// set_size keeps it, nor shorter than the shortest big panel (least); none for
// a page that fits there
fn first_height(screens: &[[i32; 4]], free: &[(i32, i32)], at: PhysicalPosition<i32>, outer: (i32, i32), height: i32, least: i32) -> Option<i32> {
    let &(_, bottom) = most_on(screens, at, outer).and_then(|i| free.get(i))?;
    let room = (bottom - at.y - (outer.1 - height)).max(least);
    (height > room).then_some(room)
}

// where 'Back' takes the panel: with the screen it opened on below the item as
// it was then (`same`), there (`then`) while that is on a screen, else below
// the item as it is now. Not as it is now first: on Windows an item among the
// hidden icons is at their chevron once they close, and on macOS anchor_for
// goes by the panel's own screen, no longer the item's once the person moved it
// there. With that screen changed the item may have moved with its taskbar: as
// it is now first, else as it opened while that is on a screen
fn back_to(screens: &[[i32; 4]], same: bool, then: Option<PhysicalPosition<i32>>, now: impl FnOnce() -> Option<PhysicalPosition<i32>>) -> Option<PhysicalPosition<i32>> {
    let then = then.filter(|a| on_one(screens, [a.x, a.y]));
    if same { then.or_else(now) } else { now().or(then) }
}

// from the big panel's left edge to where a panel this wide stands, and back
fn x_for(w: &WebviewWindow, big_x: i32, y: i32, width: i32) -> i32 {
    let big = big_width(w);
    if is_right(w, big_x, y, big) { big_x + big - width } else { big_x }
}

fn big_x_of(w: &WebviewWindow, x: i32, y: i32, width: i32) -> i32 {
    if is_right(w, x, y, width) { x + width - big_width(w) } else { x }
}

fn move_to(w: &WebviewWindow, at: PhysicalPosition<i32>) {
    if w.outer_position().is_ok_and(|p| p == at) {
        return;
    }
    *EXPECTED.lock().expect("panel") = Some(at);
    let _ = w.set_position(at);
}

// where Pulse itself moves a page this big: kept on the free part of the screen
// it lands on, all but the frame Windows keeps unseen round it. On macOS, where
// screens of different scales overlap in pixels, the screen it is on
fn within(w: &WebviewWindow, to: PhysicalPosition<i32>, width: i32, height: i32) -> PhysicalPosition<i32> {
    let (Ok(outer), Ok(inner)) = (w.outer_position(), w.inner_position()) else { return to };
    let (left, top) = (inner.x - outer.x, inner.y - outer.y);
    let lands_on = if cfg!(windows) { monitor_at(w, to.x + left + width / 2, to.y + top + height / 2) } else { None };
    let Some(area) = lands_on.or_else(|| w.current_monitor().ok().flatten()).map(|m| *m.work_area()) else { return to };
    let (x0, y0) = (area.position.x, area.position.y);
    let (x1, y1) = (x0 + area.size.width as i32, y0 + area.size.height as i32);
    PhysicalPosition::new((to.x + left).min(x1 - width).max(x0) - left, (to.y + top).min(y1 - height).max(y0) - top)
}

// big again: the page hears it and grows (App.tsx)
fn make_big(app: &AppHandle) {
    if COMPACT.swap(false, Ordering::SeqCst) {
        let _ = app.emit_to(LABEL, "size", "big");
    }
}

// a click on the item: small it grows, big it closes, closed it opens big
pub fn toggle(app: &AppHandle, rect: Rect) {
    let Some(w) = window(app) else { return };
    if w.is_visible().unwrap_or(false) {
        if COMPACT.load(Ordering::SeqCst) {
            make_big(app);
            let _ = w.set_focus();
        } else {
            hide(app);
        }
        return;
    }
    if CLOSED_AT.lock().expect("panel").is_some_and(|at| at.elapsed() < Duration::from_millis(300)) {
        return;
    }
    show(app, &w, rect);
}

// a click on a notification about no one session: the panel, big
pub fn open_big(app: &AppHandle) {
    let Some(w) = window(app) else { return };
    if w.is_visible().unwrap_or(false) {
        make_big(app);
        let _ = w.set_focus();
    } else if let Some(rect) = crate::tray::rect(app) {
        show(app, &w, rect);
    }
}

fn show(app: &AppHandle, w: &WebviewWindow, rect: Rect) {
    make_big(app);
    let anchor = anchor_for(w, rect);
    *ANCHOR.lock().expect("panel") = Some(anchor);
    if cfg!(windows) {
        *LAYOUT.lock().expect("panel") = layout(&w.available_monitors().unwrap_or_default(), anchor);
    }
    // copied out before the screens are asked: off the main thread (a click on a
    // notification) that waits on the main thread, which may want the store
    let place = STORE.lock().expect("store").panel_at;
    let placed = place.filter(|&at| on_screen(w, at));
    if AT_PLACE.swap(placed.is_some(), Ordering::SeqCst) != placed.is_some() {
        let _ = app.emit_to(LABEL, "placed", placed.is_some());
    }
    let [x, y] = placed.unwrap_or([anchor.x, anchor.y]);
    let mut to = PhysicalPosition::new(x_for(w, x, y, current_width(w)), y);
    // put too low for it as big as it is, it opens risen just enough, as it
    // stood when it closed: risen, or where it was put. On macOS, where screens
    // of different scales overlap in pixels, the measure that the snapshot below
    // brings does it (set_size)
    if let (Some(_), Ok(page), Ok(outer)) = (placed.filter(|_| cfg!(windows)), w.inner_size(), w.outer_size()) {
        let monitors = w.available_monitors().unwrap_or_default();
        let scale = w.scale_factor().unwrap_or(2.0);
        let least = (MIN_HEIGHT * scale).round() as i32;
        let risen = *RISEN.lock().expect("panel");
        to = opened(&screens(&monitors), &free_parts(&monitors), risen, to, (outer.width as i32, outer.height as i32), (page.width as i32, page.height as i32), least);
        // before its first measure after a start the page is as tall as the
        // window was made: no taller than the room below its head
        if let Some(room) = first_height(&screens(&monitors), &free_parts(&monitors), to, (outer.width as i32, outer.height as i32), page.height as i32, least) {
            let _ = w.set_size(PhysicalSize::new(page.width, room as u32));
            // the place by the item keeps its foot, as set_size keeps it: back
            // there the panel stands by the item
            if ABOVE.load(Ordering::SeqCst) {
                let s = monitor_at(w, anchor.x + big_width(w) / 2, anchor.y).map_or(scale, |m| m.scale_factor());
                *ANCHOR.lock().expect("panel") = Some(PhysicalPosition::new(anchor.x, anchor.y + ((page.height as i32 - room) as f64 / scale * s).round() as i32));
            }
        }
    }
    *RISEN.lock().expect("panel") = (to.y < y).then_some(y);
    move_to(w, to);
    let _ = w.show();
    let _ = w.set_focus();
    crate::push_last(app);
}

// below the item in the menu bar, kept within the part of the panel's screen
// that the menu bar and the Dock leave free. The panel's own screen: tao finds
// a screen on macOS by points, and the item's rect is in pixels
#[cfg(not(windows))]
fn anchor_for(w: &WebviewWindow, rect: Rect) -> PhysicalPosition<i32> {
    let scale = w.scale_factor().unwrap_or(2.0);
    let item = rect.position.to_physical::<f64>(scale);
    let item_size = rect.size.to_physical::<f64>(scale);
    let size = big_width(w) as f64;
    let mut x = item.x + item_size.width / 2.0 - size / 2.0;
    if let Ok(Some(monitor)) = w.current_monitor().or_else(|_| w.primary_monitor()) {
        let area = monitor.work_area();
        let (left, right) = (area.position.x as f64, area.position.x as f64 + area.size.width as f64);
        x = x.min(right - size - 8.0 * scale).max(left + 8.0 * scale);
    }
    ABOVE.store(false, Ordering::SeqCst);
    PhysicalPosition::new(x.round() as i32, (item.y + item_size.height + 6.0 * scale).round() as i32)
}

// by the item in the taskbar, measured for the item's screen, which need not be
// the one the panel was last on
#[cfg(windows)]
fn anchor_for(w: &WebviewWindow, rect: Rect) -> PhysicalPosition<i32> {
    let scale = w.scale_factor().unwrap_or(2.0);
    let item = rect.position.to_physical::<f64>(scale);
    let item_size = rect.size.to_physical::<f64>(scale);
    let (cx, cy) = (item.x + item_size.width / 2.0, item.y + item_size.height / 2.0);
    let monitor = w.monitor_from_point(cx, cy).ok().flatten().or_else(|| w.primary_monitor().ok().flatten());
    let ms = monitor.as_ref().map_or(scale, |m| m.scale_factor());
    let size = (WIDTH * ms).round();
    let gap = 6.0 * ms;
    let panel = w.outer_size().map_or(560.0 * scale, |s| s.height as f64) / scale * ms;
    // below the item in a taskbar at the screen's head
    let mut x = cx - size / 2.0;
    let mut y = item.y + item_size.height + gap;
    let mut above = false;
    if let Some(monitor) = monitor {
        // kept within the part of the item's screen that the taskbar leaves free
        let area = monitor.work_area();
        let (left, top) = (area.position.x as f64, area.position.y as f64);
        let (right, bottom) = (left + area.size.width as f64, top + area.size.height as f64);
        if (top..bottom).contains(&cy) && !(left..right).contains(&cx) {
            // beside it in a taskbar at the screen's side; at the item's height,
            // its foot level with the item's in the lower half
            x = if cx < left { left + gap } else { right - size - gap };
            above = cy > top + (bottom - top) / 2.0;
            y = if above { (item.y + item_size.height).min(bottom - gap) - panel } else { item.y.max(top + gap) };
            y = y.max(top);
        } else {
            x = x.min(right - size - 8.0 * ms).max(left + 8.0 * ms);
            // above it in a taskbar at the screen's foot
            if cy > monitor.position().y as f64 + monitor.size().height as f64 / 2.0 {
                y = (item.y - panel - gap).max(top);
                above = true;
            }
        }
    }
    ABOVE.store(above, Ordering::SeqCst);
    PhysicalPosition::new(x.round() as i32, y.round() as i32)
}

pub fn is_visible(app: &AppHandle) -> bool {
    window(app).is_some_and(|w| w.is_visible().unwrap_or(false))
}

pub fn on_blur(app: &AppHandle) {
    if !PINNED.load(Ordering::SeqCst) && !COMPACT.load(Ordering::SeqCst) && !in_front(app) {
        // a panel hidden already did not just close: on Windows the unseen panel
        // loses the focus once at the start, as its page is made, and a click on
        // the item just after opens it. Its place is kept all the same
        let shown = is_visible(app);
        hide(app);
        if shown {
            *CLOSED_AT.lock().expect("panel") = Some(Instant::now());
        }
    }
}

// on Windows a press on the head, to move the panel, takes the focus from the
// page while the panel stays the window in front: that is no click outside
#[cfg(windows)]
fn in_front(app: &AppHandle) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    window(app).is_some_and(|w| w.hwnd().is_ok_and(|h| h.0 == unsafe { GetForegroundWindow() }))
}

#[cfg(not(windows))]
fn in_front(_app: &AppHandle) -> bool {
    false
}

// the panel moved: away from its place below the item, the person put it
// somewhere; the panel hears whether it has a place of its own
pub fn on_moved(app: &AppHandle, at: PhysicalPosition<i32>) {
    let Some(w) = window(app).filter(|w| w.is_visible().unwrap_or(false)) else { return };
    let near = |a: PhysicalPosition<i32>, b: PhysicalPosition<i32>| (a.x - b.x).abs() <= 2 && (a.y - b.y).abs() <= 2;
    {
        let mut expected = EXPECTED.lock().expect("panel");
        if expected.is_some_and(|e| near(e, at)) {
            *expected = None;
            return;
        }
    }
    // moved by the person, it stands where they put it
    *RISEN.lock().expect("panel") = None;
    let big = PhysicalPosition::new(big_x_of(&w, at.x, at.y, current_width(&w)), at.y);
    let is_anchor = ANCHOR.lock().expect("panel").is_some_and(|a| near(a, big));
    if !is_anchor {
        STORE.lock().expect("store").panel_at = Some([big.x, big.y]);
        if !AT_PLACE.swap(true, Ordering::SeqCst) {
            let _ = app.emit_to(LABEL, "placed", true);
        }
    }
}

// whether it stands at a place of its own ('Back' shows): opened by the item, as
// the screen of its place is gone, it is back there already, and the place is
// kept for when that screen comes back
pub fn is_placed() -> bool {
    AT_PLACE.load(Ordering::SeqCst)
}

// back below the item
pub fn reset_place(app: &AppHandle) {
    STORE.lock().expect("store").panel_at = None;
    *RISEN.lock().expect("panel") = None;
    AT_PLACE.store(false, Ordering::SeqCst);
    keep_place();
    if let Some(w) = window(app) {
        // worked out again only when needed: anchor_for sets ABOVE too. On macOS,
        // where it goes by the panel's own screen, that screen counts as it was
        let then = *ANCHOR.lock().expect("panel");
        let monitors = w.available_monitors().unwrap_or_default();
        let same = !cfg!(windows) || *LAYOUT.lock().expect("panel") == then.and_then(|then| layout(&monitors, then));
        let anchor = back_to(&screens(&monitors), same, then, || {
            let anchor = anchor_for(&w, crate::tray::rect(app)?);
            if cfg!(windows) {
                *LAYOUT.lock().expect("panel") = layout(&monitors, anchor);
            }
            Some(anchor)
        });
        if let Some(anchor) = anchor {
            // kept before the move: on Windows the move calls on_moved at once,
            // which reads it too, again if the move crosses into a screen of
            // another scale
            *ANCHOR.lock().expect("panel") = Some(anchor);
            move_to(&w, PhysicalPosition::new(x_for(&w, anchor.x, anchor.y, current_width(&w)), anchor.y));
        }
    }
    let _ = app.emit_to(LABEL, "placed", false);
}

// full width unless a width is given (small: as wide as the counts); no taller
// than the free part of its screen leaves, above its foot when it grows upward,
// below its head else. Sizes are the page's; the frame Windows keeps unseen
// round it is the same at every size
pub fn set_size(app: &AppHandle, width: Option<f64>, height: f64) {
    let Some(w) = window(app) else { return };
    let min = if width.is_some() { 24.0 } else { MIN_HEIGHT };
    let width = width.map_or(WIDTH, |w| w.clamp(80.0, 640.0));
    let (Ok(at), Ok(page), Ok(outer)) = (w.outer_position(), w.inner_size(), w.outer_size()) else { return };
    let (old, old_height) = (page.width as i32, page.height as i32);
    let frame = outer.height as i32 - old_height;
    let scale = w.scale_factor().unwrap_or(2.0);
    let above = ABOVE.load(Ordering::SeqCst);
    // at its place, as show() found it, not by the item with its place gone
    let placed = AT_PLACE.load(Ordering::SeqCst);
    let grows_up = above && !placed;
    let area = w.current_monitor().ok().flatten().or_else(|| w.primary_monitor().ok().flatten()).map(|m| *m.work_area());
    let (top, bottom) = area.map_or((i32::MIN, i32::MAX), |a| (a.position.y, a.position.y + a.size.height as i32));
    // growing upward its foot stays, but never below the free part of the screen
    let foot = (at.y + old_height + frame).min(bottom);
    let room = area.map_or(MAX_HEIGHT, |_| {
        let px = if grows_up { foot - top } else { bottom - at.y };
        (px - frame) as f64 / scale
    });
    let height = height.min(room).clamp(min, MAX_HEIGHT);
    let _ = w.set_size(LogicalSize::new(width, height));
    let (new, new_height) = ((width * scale).round() as i32, (height * scale).round() as i32);
    let y = if grows_up { (foot - frame - new_height).max(top) } else { at.y };
    // the place by the item keeps its foot too, placed or not: back there the
    // panel stands by the item
    let anchor = *ANCHOR.lock().expect("panel");
    if let Some(anchor) = anchor.filter(|_| above) {
        let anchor_y = if grows_up {
            y
        } else {
            let by_item = monitor_at(&w, anchor.x + big_width(&w) / 2, anchor.y);
            let s = by_item.as_ref().map_or(scale, |m| m.scale_factor());
            let top = by_item.map_or(top, |m| m.work_area().position.y);
            (anchor.y + ((old_height - new_height) as f64 / scale * s).round() as i32).max(top)
        };
        *ANCHOR.lock().expect("panel") = Some(PhysicalPosition::new(anchor.x, anchor_y));
    }
    let monitors = w.available_monitors().unwrap_or_default();
    // placed, it goes by the head it rose from, else by where it stands
    let from = placed.then(|| RISEN.lock().expect("panel").unwrap_or(at.y));
    let to = resized(&screens(&monitors), (top, bottom), from, at, frame, (old, old_height), (new, new_height));
    *RISEN.lock().expect("panel") = from.filter(|&from| to.y < from);
    let to = if grows_up { PhysicalPosition::new(to.x, y) } else { to };
    if to != at {
        // a place the person chose keeps its edge, wherever they put it
        move_to(&w, if placed { to } else { within(&w, to, new, new_height) });
    }
}

// light or dark as chosen, or as the system is; the page follows, and so does
// the material on macOS and on Windows from build 22523, where it goes by the
// dark-mode attribute set_theme sets. Before that build Windows draws the
// acrylic with the fixed light tint in tauri.windows.conf.json
pub fn set_appearance(app: &AppHandle, appearance: Appearance) {
    if let Some(w) = window(app) {
        let _ = w.set_theme(match appearance {
            Appearance::System => None,
            Appearance::Light => Some(Theme::Light),
            Appearance::Dark => Some(Theme::Dark),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // three screens: two side by side and one under their seam, listed between them
    const SCREENS: [[i32; 4]; 3] = [[0, 0, 2560, 1440], [1260, 1440, 2560, 720], [2560, 0, 2560, 1440]];
    // the frame round the page: how much taller the window is than its page
    const FRAME: i32 = 9;

    // the free part of a screen, above a taskbar at its foot
    fn free(screen: usize) -> (i32, i32) {
        let [_, y, _, height] = SCREENS[screen];
        (y, y + height - 48)
    }

    fn at(x: i32, y: i32) -> PhysicalPosition<i32> {
        PhysicalPosition::new(x, y)
    }

    #[test]
    fn a_screen_is_found_by_both_axes_not_by_its_column() {
        // a panel centred at 3030 on the right screen is in that screen's left half
        assert_eq!(screen_at(&SCREENS, 3030, 800), Some(2));
        assert!(3030 < SCREENS[2][0] + SCREENS[2][2] / 2);
        assert_eq!(screen_at(&SCREENS, 3030, 1500), Some(1));
        assert_eq!(screen_at(&SCREENS, 1300, 1439), Some(0));
        assert_eq!(screen_at(&SCREENS, 1300, 1440), Some(1));
    }

    #[test]
    fn a_point_off_every_screen_takes_the_nearest_in_its_column() {
        assert_eq!(screen_at(&SCREENS, 3030, -20), Some(2));
        assert_eq!(screen_at(&SCREENS, 3030, 2400), Some(1));
        assert_eq!(screen_at(&SCREENS, 5200, 800), None);
        assert_eq!(screen_at(&[], 0, 0), None);
    }

    #[test]
    fn a_place_flush_with_the_left_edge_of_a_screen_is_on_it_and_one_on_a_screen_that_is_gone_is_not() {
        // the corner in the unseen frame, 7 px left of what shows: flush with the
        // left screen's left edge, with the right one's at the seam, with that of
        // the one below; and across the seam, a little more on the left screen
        for place in [[-7, 300], [2553, 300], [1253, 1460], [2530, 300]] {
            assert!(on_one(&SCREENS, place));
        }
        assert!(!on_one(&SCREENS, [6000, 200]));
        assert!(!on_one(&SCREENS, [-50, 300]));
        // the right screen gone: a place on it, and the place below the item
        // worked out before (by the item on a 2560 px screen, of which one 1920
        // px wide is left)
        assert!(!on_one(&SCREENS[..1], [3000, 200]));
        assert!(!on_one(&SCREENS[..1], [2553, 300]));
        assert!(!on_one(&[[0, 0, 1920, 1080]], [2083, 617]));
    }

    #[test]
    fn as_big_as_it_was_a_panel_stays_where_it_is() {
        // bars by the middle of the left screen, where a bar's side and that of
        // the big panel at its edge differ: set_size once took the side of each
        for x in [1100, 1140] {
            assert_eq!(resized(&SCREENS, free(0), Some(600), at(x, 600), FRAME, (330, 36), (330, 36)), at(x, 600));
        }
        // round the middles of the left screen and of the one below, placed or
        // not, at its place or risen from it, wherever its foot is in the free
        // part; not placed, also where it is not
        for (screen, y) in [(0, 466), (0, 1380), (1, 1500), (1, 2100)] {
            for size in [(150, 36), (330, 36), (420, 200), (420, 694)] {
                let fits = y + FRAME + size.1 <= free(screen).1;
                for from in [None, Some(y), Some(y + 40)].into_iter().filter(|from| fits || from.is_none()) {
                    for x in 900..1700 {
                        assert_eq!(resized(&SCREENS, free(screen), from, at(x, y), FRAME, size, size), at(x, y));
                    }
                }
            }
        }
    }

    #[test]
    fn as_big_as_it_was_a_placed_panel_with_its_foot_past_the_free_part_rises_just_enough() {
        // opened again at its low place as big as it was: on macOS the measure
        // that follows lifts it, as show() does on Windows
        assert_eq!(resized(&SCREENS, free(2), Some(1347), at(4410, 1347), FRAME, (420, 200), (420, 200)), at(4410, 1183));
        // risen, as the free part shrinks under it (a taller taskbar), and so as
        // often as it is measured again
        let shrunk = (0, free(2).1 - 100);
        let big = resized(&SCREENS, shrunk, Some(1347), at(4410, 1183), FRAME, (420, 200), (420, 200));
        assert_eq!(big, at(4410, 1083));
        assert_eq!(big.y + FRAME + 200, shrunk.1);
        assert_eq!(resized(&SCREENS, shrunk, Some(1347), big, FRAME, (420, 200), (420, 200)), big);
        // at its place at the foot of the screen below
        assert_eq!(resized(&SCREENS, free(1), Some(2000), at(2000, 2000), FRAME, (420, 200), (420, 200)), at(2000, 1903));
    }

    #[test]
    fn with_the_free_part_grown_or_shrunk_small_again_the_bar_goes_back_as_far_as_it_fits() {
        // risen from its place at 1347, then the free part grows (the taskbar
        // hidden): small again, the bar is back at its place
        assert_eq!(resized(&SCREENS, (0, 1440), Some(1347), at(4410, 1183), FRAME, (420, 200), (330, 36)), at(4500, 1347));
        // or shrinks: the bar does not fit at its place, and stays just above it
        let shrunk = (0, free(2).1 - 100);
        assert_eq!(resized(&SCREENS, shrunk, Some(1347), at(4410, 1083), FRAME, (420, 200), (330, 36)), at(4500, 1247));
    }

    // where a page this tall opens at `at`, in a window 436 px wide and FRAME taller,
    // closed risen from a head (RISEN) or not
    fn open(risen: Option<i32>, at: PhysicalPosition<i32>, height: i32) -> PhysicalPosition<i32> {
        opened(&SCREENS, &[free(0), free(1), free(2)], risen, at, (436, height + FRAME), (420, height), 200)
    }

    #[test]
    fn a_window_is_on_the_screen_that_holds_most_of_it() {
        // a big panel low on the left screen, over the one below
        assert_eq!(most_on(&SCREENS, at(1304, 1300), (436, 629)), Some(1));
        assert_eq!(most_on(&SCREENS, at(1304, 1183), (436, 629)), Some(1));
        assert_eq!(most_on(&SCREENS, at(1304, 1183), (436, 209)), Some(0));
        assert_eq!(most_on(&SCREENS, at(2400, 600), (436, 629)), Some(2));
        assert_eq!(most_on(&SCREENS, at(5200, 800), (436, 629)), None);
    }

    #[test]
    fn opened_at_a_place_too_low_for_it_a_big_panel_rises_just_enough() {
        // closed by a click elsewhere while risen, and opened again at its place;
        // or opened there after a start
        assert_eq!(open(Some(1347), at(4410, 1347), 200), at(4410, 1183));
        assert_eq!(open(None, at(4410, 1347), 200), at(4410, 1183));
        // at the foot of the screen below; with room below its head it stays
        assert_eq!(open(None, at(2000, 2000), 200), at(2000, 1903));
        assert_eq!(open(None, at(4410, 1000), 383), at(4410, 1000));
        // taller than the room below its head (the first opening after a start,
        // before the page is measured): at its place set_size makes it as tall as
        // the room, and only past the shortest big panel's room does it rise
        assert_eq!(open(None, at(804, 1033), 644), at(804, 1033));
        assert_eq!(open(None, at(4410, 1347), 644), at(4410, 1183));
    }

    #[test]
    fn opened_low_on_the_left_screen_over_the_one_below_a_big_panel_stands_as_it_closed() {
        // the bar made big there rose on the screen of its head, though most of
        // the big panel at its place is below: closed risen, it opens risen
        assert_eq!(open(Some(1347), at(1410, 1347), 200), at(1410, 1183));
        // put there big, mostly on the screen below, it stayed where it was put,
        // as set_size goes by that screen: there it opens, however tall
        assert_eq!(open(None, at(1304, 1300), 620), at(1304, 1300));
        assert_eq!(open(None, at(1304, 1250), 620), at(1304, 1250));
        assert_eq!(open(None, at(1304, 1340), 200), at(1304, 1340));
        // after a start, risen or not before, as set_size measures it at its place
        assert_eq!(open(None, at(1410, 1347), 200), at(1410, 1347));
        assert_eq!(open(None, at(1304, 1347), 644), at(1304, 1347));
        // risen from another head than that of its place, it opens as one that
        // did not rise
        assert_eq!(open(Some(1300), at(1410, 1347), 200), at(1410, 1347));
        // a little higher, mostly on the screen of its head, it rises on that one
        assert_eq!(open(None, at(1304, 1330), 200), at(1304, 1183));
    }

    #[test]
    fn opened_after_a_start_a_page_not_yet_measured_is_no_taller_than_the_room_below_its_head() {
        let free = [free(0), free(1), free(2)];
        // risen at the foot of the right screen, as tall as the window was made:
        // as tall as the room, that of the shortest big panel, and never shorter
        assert_eq!(first_height(&SCREENS, &free, at(4304, 1183), (436, 629), 620, 200), Some(200));
        assert_eq!(first_height(&SCREENS, &free, at(4410, 1347), (436, 629), 620, 200), Some(200));
        // with room below its head it stays, as does a page as short as the room,
        // or a bar (made big, it grows at the measure)
        assert_eq!(first_height(&SCREENS, &free, at(4304, 600), (436, 629), 620, 200), None);
        assert_eq!(first_height(&SCREENS, &free, at(4304, 1183), (436, 209), 200, 200), None);
        assert_eq!(first_height(&SCREENS, &free, at(4500, 1347), (346, 45), 36, 200), None);
        // low on the left screen, mostly over the one below: by that one's room
        assert_eq!(first_height(&SCREENS, &free, at(1304, 1300), (436, 629), 620, 200), None);
    }

    #[test]
    fn back_goes_below_the_item_as_it_opened_there_while_that_is_on_a_screen_else_as_it_is_now() {
        // an icon among the hidden ones, opened by it: closed, they put it
        // at their chevron. Nor is it asked again: anchor_for sets ABOVE too
        assert_eq!(back_to(&SCREENS, true, Some(at(1996, 540)), || panic!("asked again")), Some(at(1996, 540)));
        // macOS: moved onto the screen right of the item's, by which anchor_for
        // goes now
        let mac = [[0, 0, 2560, 1440], [2560, 0, 1920, 1080]];
        assert_eq!(back_to(&mac, true, Some(at(1812, 54)), || panic!("asked again")), Some(at(1812, 54)));
        // where it opened is off the screens (on macOS, undocked to a smaller
        // one), so below the item as it is now; none if that is not found either
        let laptop = [[0, 0, 1920, 1080]];
        assert_eq!(back_to(&laptop, true, Some(at(2096, 821)), || Some(at(1492, 461))), Some(at(1492, 461)));
        assert_eq!(back_to(&laptop, true, Some(at(2096, 821)), || None), None);
        assert_eq!(back_to(&SCREENS, true, None, || Some(at(1996, 540))), Some(at(1996, 540)));
    }

    #[test]
    fn with_the_screens_changed_back_goes_below_the_item_as_it_is_now_else_as_it_opened_while_that_is_on_a_screen() {
        // undocked to a laptop screen big enough to hold where it opened by the
        // icon, or the taskbar moved to the head: below the icon as it is now
        let laptop = [[0, 0, 2880, 1800]];
        assert_eq!(back_to(&laptop, false, Some(at(2096, 821)), || Some(at(2452, 1181))), Some(at(2452, 1181)));
        assert_eq!(back_to(&SCREENS, false, Some(at(2096, 821)), || Some(at(2096, 50))), Some(at(2096, 50)));
        // so too an icon among the hidden ones: by their chevron
        assert_eq!(back_to(&SCREENS, false, Some(at(1996, 540)), || Some(at(2076, 757))), Some(at(2076, 757)));
        // the icon not found: as it opened, while that is on a screen
        assert_eq!(back_to(&laptop, false, Some(at(2096, 821)), || None), Some(at(2096, 821)));
        assert_eq!(back_to(&[[0, 0, 1920, 1080]], false, Some(at(2096, 821)), || None), None);
    }

    #[test]
    fn made_big_a_bar_at_the_foot_of_its_screen_rises_just_enough() {
        // parked at the foot of the right screen, on its right half: the least a
        // big panel is (200) does not fit below its head
        let (_, foot) = free(2);
        let big = resized(&SCREENS, free(2), Some(1347), at(4500, 1347), FRAME, (330, 36), (420, 200));
        assert_eq!(big, at(4410, 1183));
        assert_eq!(big.y + FRAME + 200, foot);
        // at the foot of the left screen, over the one below: it stays on its own
        assert_eq!(resized(&SCREENS, free(0), Some(1347), at(1500, 1347), FRAME, (330, 36), (420, 200)), at(1410, 1183));
        // not placed, the panel is left to within()
        assert_eq!(resized(&SCREENS, free(2), None, at(4500, 1347), FRAME, (330, 36), (420, 200)), at(4410, 1347));
    }

    #[test]
    fn small_again_the_bar_is_back_at_its_place() {
        let big = resized(&SCREENS, free(2), Some(1347), at(4500, 1347), FRAME, (330, 36), (420, 200));
        let bar = resized(&SCREENS, free(2), Some(1347), big, FRAME, (420, 200), (330, 36));
        assert_eq!(bar, at(4500, 1347));
        // and big again just as high, as often as it goes
        assert_eq!(resized(&SCREENS, free(2), Some(1347), bar, FRAME, (330, 36), (420, 200)), big);
    }

    #[test]
    fn with_room_below_it_a_placed_panel_keeps_its_head() {
        // a bar higher up made big, as tall as the room below it, or as the least
        // a big panel is; and one away from its place, by the item after the
        // screen of its place went away, goes by where it stands
        assert_eq!(resized(&SCREENS, free(2), Some(1000), at(4500, 1000), FRAME, (330, 36), (420, 383)), at(4410, 1000));
        assert_eq!(resized(&SCREENS, free(2), Some(1183), at(4500, 1183), FRAME, (330, 36), (420, 200)), at(4410, 1183));
        assert_eq!(resized(&SCREENS, free(0), Some(466), at(1996, 466), FRAME, (420, 694), (330, 36)), at(2086, 466));
    }

    #[test]
    fn smaller_or_bigger_it_keeps_the_edge_nearer_the_side() {
        // a big panel by the item, on the right half of the left screen
        assert_eq!(resized_x(&SCREENS, 1996, 466, 420, 330), 2086);
        assert_eq!(resized_x(&SCREENS, 2086, 466, 330, 420), 1996);
        // on the left half the left edge stays
        assert_eq!(resized_x(&SCREENS, 300, 466, 420, 330), 300);
        assert_eq!(resized_x(&SCREENS, 300, 466, 330, 420), 300);
    }
}
