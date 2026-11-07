// The menu bar item: one coloured symbol per open state with its count
// (blocked, question, result) and for the commands worth a look (an
// hourglass), a grey check when nothing needs the person;
// what runs does not show here, only in the panel. The image is drawn only
// when one of these counts changes: each new image
// makes AppKit lay the item out anew, which an animation would do several
// times a second.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Wry};
use tauri_plugin_autostart::ManagerExt;

use crate::items::Counts;
use crate::panel;

const ID: &str = "main";
const HEIGHT: usize = 36;

struct Mask {
    w: usize,
    h: usize,
    alpha: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Mask {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().expect("tray mask");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("tray mask");
    let channels = info.color_type.samples();
    let alpha = buf[..info.buffer_size()].chunks(channels).map(|px| px[channels - 1]).collect();
    Mask { w: info.width as usize, h: info.height as usize, alpha }
}

struct Masks {
    blocked: Mask,
    waiting: Mask,
    review: Mask,
    commands: Mask,
    done: Mask,
    digits: Vec<Mask>,
}

static MASKS: LazyLock<Masks> = LazyLock::new(|| Masks {
    blocked: decode(include_bytes!("../icons/tray/blocked.png")),
    waiting: decode(include_bytes!("../icons/tray/waiting.png")),
    review: decode(include_bytes!("../icons/tray/review.png")),
    commands: decode(include_bytes!("../icons/tray/commands.png")),
    done: decode(include_bytes!("../icons/tray/done.png")),
    digits: [
        &include_bytes!("../icons/tray/digit0.png")[..],
        include_bytes!("../icons/tray/digit1.png"),
        include_bytes!("../icons/tray/digit2.png"),
        include_bytes!("../icons/tray/digit3.png"),
        include_bytes!("../icons/tray/digit4.png"),
        include_bytes!("../icons/tray/digit5.png"),
        include_bytes!("../icons/tray/digit6.png"),
        include_bytes!("../icons/tray/digit7.png"),
        include_bytes!("../icons/tray/digit8.png"),
        include_bytes!("../icons/tray/digit9.png"),
    ]
    .iter()
    .map(|b| decode(b))
    .collect(),
});

const RED: [u8; 3] = [0xff, 0x4d, 0x55];
const AMBER: [u8; 3] = [0xf5, 0xa5, 0x24];
const VIOLET: [u8; 3] = [0xa4, 0x6b, 0xf0];
const TEAL: [u8; 3] = [0x1f, 0xb8, 0xa5];

struct Canvas {
    w: usize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(w: usize) -> Canvas {
        Canvas { w, rgba: vec![0; w * HEIGHT * 4] }
    }

    // a mask in a colour at (x, y)
    fn draw(&mut self, mask: &Mask, x: usize, y: usize, color: [u8; 3]) {
        for my in 0..mask.h {
            for mx in 0..mask.w {
                let a = mask.alpha[my * mask.w + mx];
                let (px, py) = (x + mx, y + my);
                if a == 0 || px >= self.w || py >= HEIGHT {
                    continue;
                }
                let i = (py * self.w + px) * 4;
                self.rgba[i..i + 3].copy_from_slice(&color);
                self.rgba[i + 3] = self.rgba[i + 3].max(a);
            }
        }
    }
}

// a symbol and its count side by side
struct Chip {
    mask: &'static Mask,
    color: [u8; 3],
    count: Option<usize>,
}

fn chips(counts: &Counts) -> Vec<Chip> {
    let m = &*MASKS;
    let mut list = Vec::new();
    for (n, mask, color) in [(counts.blocked, &m.blocked, RED), (counts.waiting, &m.waiting, AMBER), (counts.review, &m.review, VIOLET), (counts.commands, &m.commands, TEAL)] {
        if n > 0 {
            list.push(Chip { mask, color, count: Some(n) });
        }
    }
    list
}

#[cfg(not(windows))]
const GAP_DIGIT: usize = 3;
#[cfg(not(windows))]
const GAP_CHIP: usize = 14;

#[cfg(not(windows))]
fn render(chips: &[Chip]) -> Image<'static> {
    let m = &*MASKS;
    let digits = |n: usize| n.to_string().bytes().map(|b| &m.digits[(b - b'0') as usize]).collect::<Vec<_>>();
    let width: usize = chips
        .iter()
        .map(|c| c.mask.w + c.count.map_or(0, |n| GAP_DIGIT + digits(n).iter().map(|d| d.w + 1).sum::<usize>()))
        .sum::<usize>()
        + GAP_CHIP * chips.len().saturating_sub(1);
    let mut canvas = Canvas::new(width.max(1));
    let mut x = 0;
    for chip in chips {
        canvas.draw(chip.mask, x, (HEIGHT - chip.mask.h) / 2, chip.color);
        x += chip.mask.w;
        if let Some(n) = chip.count {
            x += GAP_DIGIT;
            for d in digits(n) {
                canvas.draw(d, x, 0, chip.color);
                x += d.w + 1;
            }
        }
        x += GAP_CHIP;
    }
    Image::new_owned(canvas.rgba, width.max(1) as u32, HEIGHT as u32)
}

// all calm: a check in the menu bar's own colour (a template image); the
// Windows tray takes no template, a grey reads on a light and a dark taskbar
fn calm() -> Image<'static> {
    let mut canvas = Canvas::new(if cfg!(windows) { HEIGHT } else { MASKS.done.w });
    let x = if cfg!(windows) { (HEIGHT - MASKS.done.w) / 2 } else { 0 };
    canvas.draw(&MASKS.done, x, (HEIGHT - MASKS.done.h) / 2, if cfg!(windows) { [0x9a, 0x9a, 0x9a] } else { [0, 0, 0] });
    let width = canvas.w as u32;
    Image::new_owned(canvas.rgba, width, HEIGHT as u32)
}

// the Windows tray shows a square: the most urgent symbol, and on it a badge
// with the count of everything open; the tooltip names each count (tooltip)
#[cfg(windows)]
fn render_square(chips: &[Chip]) -> Image<'static> {
    let m = &*MASKS;
    let mut canvas = Canvas::new(HEIGHT);
    let first = &chips[0];
    canvas.draw(first.mask, 0, 0, first.color);
    let total: usize = chips.iter().filter_map(|c| c.count).sum();
    // the digits at half size: the strongest of each two by two
    let half = |d: &Mask| {
        let (w, h) = (d.w.div_ceil(2), d.h.div_ceil(2));
        let at = |x: usize, y: usize| if x < d.w && y < d.h { d.alpha[y * d.w + x] } else { 0 };
        let alpha = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| at(2 * x, 2 * y).max(at(2 * x + 1, 2 * y)).max(at(2 * x, 2 * y + 1)).max(at(2 * x + 1, 2 * y + 1)))
            .collect();
        Mask { w, h, alpha }
    };
    let digits: Vec<Mask> = total.min(99).to_string().bytes().map(|b| half(&m.digits[(b - b'0') as usize])).collect();
    let text_w = digits.iter().map(|d| d.w + 1).sum::<usize>().saturating_sub(1);
    let (badge_w, badge_h) = ((text_w + 6).max(HEIGHT / 2), HEIGHT / 2 + 2);
    let (bx, by) = (HEIGHT - badge_w, HEIGHT - badge_h);
    // a clear ring cut from the symbol around the badge: of one colour, the two
    // would run together at the tray's size
    for py in by.saturating_sub(3)..HEIGHT {
        for px in bx.saturating_sub(3)..HEIGHT {
            let i = (py * HEIGHT + px) * 4;
            canvas.rgba[i..i + 4].fill(0);
        }
    }
    canvas.draw(&Mask { w: badge_w, h: badge_h, alpha: vec![255; badge_w * badge_h] }, bx, by, first.color);
    let mut x = bx + (badge_w - text_w) / 2;
    for d in &digits {
        canvas.draw(d, x, by + (badge_h - d.h) / 2, [0xff, 0xff, 0xff]);
        x += d.w + 1;
    }
    Image::new_owned(canvas.rgba, HEIGHT as u32, HEIGHT as u32)
}

static COUNTS: LazyLock<Mutex<Option<Counts>>> = LazyLock::new(Default::default);
// what the image shows: blocked, waiting, review, commands
static SHOWN: LazyLock<Mutex<Option<[usize; 4]>>> = LazyLock::new(Default::default);
// whether the image is a template now: switching it sets the image a second time
static IS_TEMPLATE: AtomicBool = AtomicBool::new(true);

fn draw(app: &AppHandle, counts: &Counts) {
    let Some(tray) = app.tray_by_id(ID) else { return };
    let list = chips(counts);
    let is_template = list.is_empty();
    #[cfg(windows)]
    let image = if is_template { calm() } else { render_square(&list) };
    #[cfg(not(windows))]
    let image = if is_template { calm() } else { render(&list) };
    let mut result = tray.set_icon(Some(image));
    if IS_TEMPLATE.swap(is_template, Ordering::SeqCst) != is_template {
        result = result.and_then(|_| tray.set_icon_as_template(is_template));
    }
    if let Err(error) = result {
        eprintln!("tray: {error}");
    }
}

// a new count: a new image and tooltip
pub fn show(app: &AppHandle, counts: &Counts) {
    let mut current = COUNTS.lock().expect("tray counts");
    if current.as_ref() == Some(counts) {
        return;
    }
    *current = Some(counts.clone());
    drop(current);
    // something starting or ending its run leaves the image as it is
    let shown = [counts.blocked, counts.waiting, counts.review, counts.commands];
    if SHOWN.lock().expect("tray shown").replace(shown) != Some(shown) {
        draw(app, counts);
    }
    if let Some(tray) = app.tray_by_id(ID) {
        let _ = tray.set_tooltip(Some(tooltip(counts)));
    }
}

// each count that needs the person, as the panel names it and in the image's
// order ("Pulse: 1 blocked, 2 questions"), else how many run; within the 127
// characters a Windows tooltip holds
fn tooltip(counts: &Counts) -> String {
    let named = [(counts.blocked, "blocked", "blocked"), (counts.waiting, "question", "questions"), (counts.review, "to review", "to review"), (counts.commands, "command", "commands")];
    let each: Vec<String> = named.iter().filter(|(n, ..)| *n > 0).map(|&(n, one, many)| format!("{n} {}", if n == 1 { one } else { many })).collect();
    let mut tip = match each.len() {
        0 if counts.running > 0 => format!("Pulse: {} running", counts.running),
        0 => "Pulse: all quiet".to_string(),
        _ => format!("Pulse: {}", each.join(", ")),
    };
    // all ASCII: a byte is a character
    tip.truncate(127);
    tip
}

fn autostart_enabled(app: &AppHandle) -> bool {
    match app.autolaunch().is_enabled() {
        Ok(enabled) => enabled,
        Err(error) => {
            eprintln!("autostart: {error}");
            false
        }
    }
}

fn toggle_autostart(app: &AppHandle, item: &CheckMenuItem<Wry>) {
    let launcher = app.autolaunch();
    let result = if autostart_enabled(app) { launcher.disable() } else { launcher.enable() };
    if let Err(error) = result {
        eprintln!("autostart: {error}");
    }
    let _ = item.set_checked(autostart_enabled(app));
}

// left click opens and closes the panel; right click shows a small menu
// where the item is, to open the panel below it without a click on it
pub fn rect(app: &AppHandle) -> Option<tauri::Rect> {
    app.tray_by_id(ID)?.rect().ok().flatten()
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let autostart = CheckMenuItem::with_id(app, "autostart", "Start at login", true, autostart_enabled(app), None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Pulse", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&autostart, &PredefinedMenuItem::separator(app)?, &quit])?;
    let item = autostart.clone();
    TrayIconBuilder::with_id(ID)
        .icon(calm())
        .icon_as_template(true)
        .tooltip("Pulse")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "autostart" => toggle_autostart(app, &item),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = event {
                panel::toggle(tray.app_handle(), rect);
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tooltip_names_each_count() {
        let all = Counts { blocked: 1, waiting: 2, review: 1, commands: 1, running: 3, ..Counts::default() };
        assert_eq!(tooltip(&all), "Pulse: 1 blocked, 2 questions, 1 to review, 1 command");
        assert_eq!(tooltip(&Counts { waiting: 1, commands: 4, ..Counts::default() }), "Pulse: 1 question, 4 commands");
        assert_eq!(tooltip(&Counts { running: 2, follow: 5, parked: 1, ..Counts::default() }), "Pulse: 2 running");
        assert_eq!(tooltip(&Counts::default()), "Pulse: all quiet");
        // the 127 characters of a Windows tooltip, however large the counts
        let large = Counts { blocked: 99_999, waiting: 99_999, review: 99_999, commands: 99_999, ..Counts::default() };
        assert_eq!(tooltip(&large), "Pulse: 99999 blocked, 99999 questions, 99999 to review, 99999 commands");
        let huge = Counts { blocked: usize::MAX, waiting: usize::MAX, review: usize::MAX, commands: usize::MAX, ..Counts::default() };
        assert_eq!(tooltip(&huge).encode_utf16().count(), 127);
    }
}
