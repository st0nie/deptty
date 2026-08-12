//! deptty: deepin-terminal rewritten in Rust on DTK6.
//! Terminal core: alacritty_terminal (VT parsing + screen grid) + portable-pty (PTY).
//! Rendering: QPainter cell grid, same approach as qtermwidget's TerminalDisplay.
//!
//! Structure: one window, tabs in the titlebar; each tab is a binary split tree
//! of panes. One pane = one shell = one PaintWidget + one PTY reader thread.
pub mod config;

// test harness drives dtk/alacritty types through these re-exports
pub use alacritty_terminal;
pub use dtk;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use config::Config;
use dtk::widgets::DTabBar;
use dtk::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rust_i18n::t;
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

rust_i18n::i18n!("locales", fallback = "en");

/// grid size in cells; alacritty TerminalDimensions
struct Size {
    cols: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// OSC 0/2 window title -> shared slot (reader thread writes, GUI reads per frame)
#[derive(Clone)]
pub struct TitleListener {
    slot: Arc<Mutex<Option<String>>>,
}

impl alacritty_terminal::event::EventListener for TitleListener {
    fn send_event(&self, ev: alacritty_terminal::event::Event) {
        use alacritty_terminal::event::Event;
        // empty/reset titles are ignored on purpose: shells emit reset-then-set
        // around every prompt, applying the empty one flickers the tab label
        if let Event::Title(t) = ev
            && !t.is_empty()
        {
            *self.slot.lock().unwrap() = Some(t);
        }
    }
}

type AppTerm = Term<TitleListener>;

/// shared terminal state; reader thread feeds it, GUI thread renders it
pub struct Shared {
    term: FairMutex<AppTerm>,
    /// pending OSC title (reader writes); GUI applies it after a coalesce window
    title: Arc<Mutex<Option<String>>>,
    /// qtermwidget-style title debounce: only one apply timer in flight
    title_armed: Arc<std::sync::atomic::AtomicBool>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl Shared {
    /// test hook: lock the terminal grid
    pub fn term(&self) -> &FairMutex<AppTerm> {
        &self.term
    }
    /// test hook: write bytes to the shell
    pub fn write(&self, bytes: &[u8]) {
        let _ = self.writer.lock().unwrap().write_all(bytes);
    }
}

// ---- colors: xterm palette ----

/// palette lifted from deepin-terminal's default Dark / Light colorschemes
struct Scheme {
    dark: bool,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    colors: [(u8, u8, u8); 16],
}

fn scheme() -> Scheme {
    let (r, g, b) = DApplication::palette_window_rgb();
    let light = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000 >= 128;
    if light {
        Scheme {
            dark: false,
            fg: (0, 0, 0),
            bg: (248, 248, 248),
            colors: [
                (0, 0, 0),       // black
                (178, 24, 24),   // red
                (24, 178, 24),   // green
                (178, 104, 24),  // yellow
                (24, 24, 178),   // blue
                (225, 30, 225),  // magenta
                (24, 178, 178),  // cyan
                (238, 232, 213), // white
                (104, 104, 104),
                (255, 84, 84),
                (133, 153, 0),
                (233, 233, 79),
                (52, 101, 164),
                (30, 144, 255),
                (24, 178, 178),
                (238, 232, 213),
            ],
        }
    } else {
        Scheme {
            dark: true,
            fg: (0, 205, 0), // deepin's signature green-on-dark
            bg: (37, 37, 37),
            colors: [
                (0, 0, 0),
                (178, 24, 24),
                (24, 178, 24),
                (178, 104, 24),
                (52, 101, 164),
                (225, 30, 225),
                (24, 178, 178),
                (238, 232, 213),
                (104, 104, 104),
                (255, 84, 84),
                (133, 153, 0),
                (255, 255, 84),
                (52, 101, 164),
                (30, 144, 255),
                (253, 246, 227),
                (255, 255, 255),
            ],
        }
    }
}

fn named_rgb(c: NamedColor, sc: &Scheme) -> (u8, u8, u8) {
    use NamedColor::*;
    match c {
        Foreground | BrightForeground | DimForeground => sc.fg,
        Background => sc.bg,
        Black | DimBlack => sc.colors[0],
        Red | DimRed => sc.colors[1],
        Green | DimGreen => sc.colors[2],
        Yellow | DimYellow => sc.colors[3],
        Blue | DimBlue => sc.colors[4],
        Magenta | DimMagenta => sc.colors[5],
        Cyan | DimCyan => sc.colors[6],
        White | DimWhite => sc.colors[7],
        BrightBlack => sc.colors[8],
        BrightRed => sc.colors[9],
        BrightGreen => sc.colors[10],
        BrightYellow => sc.colors[11],
        BrightBlue => sc.colors[12],
        BrightMagenta => sc.colors[13],
        BrightCyan => sc.colors[14],
        BrightWhite => sc.colors[15],
        _ => sc.fg,
    }
}

const NAMED16: [NamedColor; 16] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
];

/// cell fg/bg after video attributes: INVERSE swaps, bold fg goes bright
fn cell_colors(cell: &alacritty_terminal::term::cell::Cell) -> (Color, Color) {
    use alacritty_terminal::term::cell::Flags;
    let (mut fg, mut bg) = (cell.fg, cell.bg);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags.contains(Flags::BOLD) {
        fg = match fg {
            Color::Named(n) => Color::Named(match n {
                NamedColor::Black => NamedColor::BrightBlack,
                NamedColor::Red => NamedColor::BrightRed,
                NamedColor::Green => NamedColor::BrightGreen,
                NamedColor::Yellow => NamedColor::BrightYellow,
                NamedColor::Blue => NamedColor::BrightBlue,
                NamedColor::Magenta => NamedColor::BrightMagenta,
                NamedColor::Cyan => NamedColor::BrightCyan,
                NamedColor::White => NamedColor::BrightWhite,
                other => other,
            }),
            Color::Indexed(i @ 0..=7) => Color::Indexed(i + 8),
            other => other,
        };
    }
    (fg, bg)
}

fn rgb_of(c: Color, sc: &Scheme) -> (u8, u8, u8) {
    match c {
        Color::Named(n) => named_rgb(n, sc),
        Color::Indexed(i @ 0..=15) => named_rgb(NAMED16[i as usize], sc),
        Color::Indexed(i @ 16..=231) => {
            let i = i - 16;
            let f = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (f(i / 36), f(i / 6 % 6), f(i % 6))
        }
        Color::Indexed(i) => {
            let g = 8 + 10 * (i - 232);
            (g, g, g)
        }
        Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
    }
}

fn color_q(c: Color, sc: &Scheme) -> QColor {
    let (r, g, b) = rgb_of(c, sc);
    QColor::rgb(i32::from(r), i32::from(g), i32::from(b))
}

// ---- keyboard -> PTY bytes ----

fn key_bytes(k: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    use dtk::qt::{key, modifier};
    let ctrl = k.mods & modifier::CONTROL != 0;
    let alt = k.mods & modifier::ALT != 0;
    let shift = k.mods & modifier::SHIFT != 0;
    if ctrl && !k.text.is_empty() {
        let b = k.text.as_bytes()[0];
        // Qt hands us the C0 control byte directly for Ctrl+letter (e.g. Ctrl+L -> 0x0c)
        if b < 0x20 {
            let mut v = vec![b];
            if alt {
                v.insert(0, 0x1b); // meta
            }
            return Some(v);
        }
        if b.is_ascii_alphabetic() {
            return Some(vec![b.to_ascii_lowercase() & 0x1f]);
        }
    }
    if !k.text.is_empty() && !ctrl {
        let mut v = Vec::new();
        if alt {
            v.push(0x1b); // meta prefix (Alt+Backspace -> ESC DEL, Alt+f -> ESC f, ...)
        }
        v.extend_from_slice(k.text.as_bytes());
        return Some(v);
    }
    // xterm modifier encoding: 1 + shift(1) + alt(2) + ctrl(4)
    let m = 1 + i32::from(shift) + 2 * i32::from(alt) + 4 * i32::from(ctrl);
    // CSI 1;<mod><final> for arrows/home/end; plain sequence when unmodified
    let csi = |plain: &'static [u8], fin: u8| -> Vec<u8> {
        if m == 1 {
            plain.to_vec()
        } else {
            format!("\x1b[1;{m}{}", fin as char).into_bytes()
        }
    };
    // CSI <n>;<mod>~ for delete/insert/pageup/pagedown
    let tilde = |plain: &'static [u8], n: i32| -> Vec<u8> {
        if m == 1 {
            plain.to_vec()
        } else {
            format!("\x1b[{n};{m}~").into_bytes()
        }
    };
    // application cursor mode (DECCKM): unmodified arrows/home/end go SS3
    if app_cursor && m == 1 {
        let fin = match k.key {
            key::UP => Some(b'A'),
            key::DOWN => Some(b'B'),
            key::RIGHT => Some(b'C'),
            key::LEFT => Some(b'D'),
            key::HOME => Some(b'H'),
            key::END => Some(b'F'),
            _ => None,
        };
        if let Some(fin) = fin {
            return Some(vec![0x1b, b'O', fin]); // ESC O <final>
        }
    }
    let v = match k.key {
        key::RETURN | key::ENTER => b"\r".to_vec(),
        key::BACKSPACE => {
            if ctrl {
                b"\x08".to_vec()
            } else if alt {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        key::TAB => b"\t".to_vec(),
        key::ESCAPE => b"\x1b".to_vec(),
        key::UP => csi(b"\x1b[A", b'A'),
        key::DOWN => csi(b"\x1b[B", b'B'),
        key::RIGHT => csi(b"\x1b[C", b'C'),
        key::LEFT => csi(b"\x1b[D", b'D'),
        key::HOME => csi(b"\x1b[H", b'H'),
        key::END => csi(b"\x1b[F", b'F'),
        key::BACKTAB => b"\x1b[Z".to_vec(), // Shift+Tab is always CSI Z (xterm)
        key::DELETE => tilde(b"\x1b[3~", 3),
        key::PAGE_UP => tilde(b"\x1b[5~", 5),
        key::PAGE_DOWN => tilde(b"\x1b[6~", 6),
        key::INSERT => tilde(b"\x1b[2~", 2),
        _ => return None,
    };
    Some(v)
}

// ---- rendering ----

/// cell pixel geometry
pub struct GridGeom {
    pub cell_w: i32,
    pub cell_h: i32,
    pub ascent: i32,
}

/// one batch of same-color text on one line
/// font variants: bold/italic baked in so runs never drift off the grid
struct Fonts {
    normal: QFont,
    bold: QFont,
    italic: QFont,
    bold_italic: QFont,
}

impl Fonts {
    fn of(cfg: &config::Config) -> Self {
        let normal = make_font(cfg);
        let bold = make_font(cfg);
        bold.set_bold(true);
        let italic = make_font(cfg);
        italic.set_italic(true);
        let bold_italic = make_font(cfg);
        bold_italic.set_bold(true);
        bold_italic.set_italic(true);
        Self {
            normal,
            bold,
            italic,
            bold_italic,
        }
    }
}

/// per-run text style; runs split when any of these change
#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Color,
    bold: bool,
    italic: bool,
    dim: bool,
    deco: alacritty_terminal::term::cell::Flags, // underline variants + strikeout
    uline: Option<Color>,                        // SGR 58 underline color
}

impl RunStyle {
    fn of(cell: &alacritty_terminal::term::cell::Cell, fg: Color) -> Self {
        use alacritty_terminal::term::cell::Flags;
        Self {
            fg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            dim: cell.flags.contains(Flags::DIM),
            deco: cell.flags & (Flags::ALL_UNDERLINES | Flags::STRIKEOUT),
            uline: cell.underline_color(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_run(
    p: &Painter,
    g: &GridGeom,
    y_off: i32,
    run: &mut String,
    line: i64,
    col: usize,
    st: Option<RunStyle>,
    sc: &Scheme,
    fonts: &Fonts,
) {
    use alacritty_terminal::term::cell::Flags;
    let Some(st) = st else {
        run.clear();
        return;
    };
    if run.is_empty() {
        return;
    }
    let font = match (st.bold, st.italic) {
        (true, true) => &fonts.bold_italic,
        (true, false) => &fonts.bold,
        (false, true) => &fonts.italic,
        (false, false) => &fonts.normal,
    };
    p.set_font(font);
    let (mut r, mut g2, mut b) = rgb_of(st.fg, sc);
    if st.dim {
        // alacritty dims the fg to 2/3 intensity
        r = r * 2 / 3;
        g2 = g2 * 2 / 3;
        b = b * 2 / 3;
    }
    let qc = QColor::rgb(i32::from(r), i32::from(g2), i32::from(b));
    p.set_pen_color(&qc);
    let x = col as i32 * g.cell_w;
    let y = y_off + line as i32 * g.cell_h;
    p.draw_text_at(x, y + g.ascent, run);

    // decorations span the whole run
    let w = run.chars().count() as i32 * g.cell_w;
    if !st.deco.is_empty() {
        let lc = st
            .uline
            .map(|c| color_q(c, sc))
            .unwrap_or_else(|| color_q(st.fg, sc));
        let lw = (g.cell_h / 12).max(1);
        let uy = y + g.ascent + 1; // just under the baseline
        if st.deco.contains(Flags::UNDERLINE) {
            p.fill_rect(x, uy, w, lw, &lc);
        }
        if st.deco.contains(Flags::DOUBLE_UNDERLINE) {
            p.fill_rect(x, uy, w, 1, &lc);
            p.fill_rect(x, uy + 2, w, 1, &lc);
        }
        if st.deco.contains(Flags::DOTTED_UNDERLINE) {
            let mut dx = x;
            while dx < x + w {
                p.fill_rect(dx, uy, lw, lw, &lc);
                dx += lw * 3;
            }
        }
        if st.deco.contains(Flags::DASHED_UNDERLINE) {
            let mut dx = x;
            while dx < x + w {
                p.fill_rect(dx, uy, lw * 3, lw, &lc);
                dx += lw * 5;
            }
        }
        if st.deco.contains(Flags::UNDERCURL) {
            // sine wave, one period per cell
            p.set_pen_color(&lc);
            let amp = lw;
            let mid = uy + amp;
            let mut px = x;
            while px < x + w {
                let t0 = (px - x) as f64 / g.cell_w as f64 * std::f64::consts::TAU;
                let t1 = (px - x + 2) as f64 / g.cell_w as f64 * std::f64::consts::TAU;
                p.draw_line(
                    px,
                    mid + (t0.sin() * amp as f64) as i32,
                    px + 2,
                    mid + (t1.sin() * amp as f64) as i32,
                );
                px += 2;
            }
        }
        if st.deco.contains(Flags::STRIKEOUT) {
            p.fill_rect(x, y + g.cell_h / 2, w, lw, &lc);
        }
    }
    run.clear();
}

/// where the shell starts: -w/--work-directory, a positional dir, or a file:// URI
/// (dde-file-manager "open in terminal here" passes the dir one of these ways)
fn work_dir_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "-w" || a == "--work-directory" {
            return args.next();
        }
        if let Some(rest) = a.strip_prefix("--work-directory=") {
            return Some(rest.to_string());
        }
        if !a.starts_with('-') {
            let p = a.strip_prefix("file://").unwrap_or(&a);
            if std::path::Path::new(p).is_dir() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// scrollbar <-> scroll state. range [0, history], value = history - display_offset,
/// so the slider sits at the bottom when viewing the prompt. syncing guards feedback.
fn sync_scrollbar(term: &AppTerm, sb: ScrollBar, syncing: &std::cell::Cell<bool>) {
    let hist = term.grid().history_size() as i32;
    let offset = term.grid().display_offset() as i32;
    syncing.set(true);
    sb.set_range(0, hist);
    sb.set_page_step(term.grid().screen_lines() as i32);
    sb.set_value(hist - offset);
    syncing.set(false);
}

fn make_font(cfg: &config::Config) -> QFont {
    let f = QFont::new();
    match &cfg.font_family {
        Some(family) => f.set_family(family),
        None => f.set_monospace(),
    }
    f.set_point_size(cfg.font_size);
    // terminals need integer per-cell advances or shaped runs drift off the grid
    f.force_integer_metrics();
    f
}

fn mouse_point(x: i32, y: i32, g: &GridGeom, term: &AppTerm) -> (Point, Side) {
    let col = (x / g.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
    let row = (y / g.cell_h).clamp(0, term.grid().screen_lines() as i32 - 1);
    // the side (left/right half of the cell) is the anchor edge: dragging leftwards
    // must anchor on the right edge of the end cell, or that cell is excluded
    let side = if x % g.cell_w < g.cell_w / 2 {
        Side::Left
    } else {
        Side::Right
    };
    (
        Point::new(Line(row - term.grid().display_offset() as i32), Column(col)),
        side,
    )
}

/// SGR (1006) or X10 mouse report; x,y are 1-based screen cells.
/// b: button + mods(shift 4, alt 8, ctrl 16) + 32 motion + 64 wheel; release = 'm'/button 3
fn mouse_mods(mods: i32) -> i32 {
    (if mods & qt::modifier::SHIFT != 0 {
        4
    } else {
        0
    }) | (if mods & qt::modifier::ALT != 0 { 8 } else { 0 })
        | (if mods & qt::modifier::CONTROL != 0 {
            16
        } else {
            0
        })
}

fn mouse_report(term: &AppTerm, b: i32, col: usize, row: i32, press: bool) -> Vec<u8> {
    let (x, y) = (col as i32 + 1, row + 1);
    if term.mode().contains(TermMode::SGR_MOUSE) {
        format!("\x1b[<{b};{x};{}{}", y, if press { 'M' } else { 'm' }).into_bytes()
    } else {
        let mut v = b"\x1b[M".to_vec();
        for n in [32 + b, 32 + x, 32 + y] {
            v.push(n.clamp(0, 255) as u8); // X10 caps at 223; wide screens clip
        }
        if !press {
            v[3] = 32 + 3; // X10 release = no-button
        }
        v
    }
}

/// char-index span of the http(s):// URL covering char index `col`, if any
fn find_url_span(text: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let delim = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '"' | '\'' | '<' | '>' | '`' | '|' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    };
    let starts = |i: usize, pat: &str| chars[i..].iter().take(pat.len()).copied().eq(pat.chars());
    let mut i = 0;
    while i + 7 <= chars.len() {
        let (https, hit) = (starts(i, "https://"), starts(i, "http://"));
        if https || hit {
            let mut e = i + if https { 8 } else { 7 };
            while e < chars.len() && !delim(chars[e]) {
                e += 1;
            }
            if col >= i && col < e {
                return Some((i, e));
            }
            i = e; // don't rescan inside a URL ("http://x http://y")
        } else {
            i += 1;
        }
    }
    None
}

/// URL + its column span at screen cell (line, col); None when not on a link.
/// ponytail: single grid line; a soft-wrapped URL only matches within each fragment
fn url_at(term: &AppTerm, line: i32, col: usize) -> Option<(String, usize, usize)> {
    use alacritty_terminal::term::cell::Flags;
    let offset = term.grid().display_offset() as i64;
    // one char per cell so char index == column (spacers/\0 padding become blanks)
    let mut chars: Vec<char> = Vec::new();
    for indexed in term.grid().display_iter() {
        if i64::from(indexed.point.line.0) + offset != i64::from(line) {
            continue;
        }
        let c = indexed.cell.c;
        chars.push(
            if c == '\0' || indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                ' '
            } else {
                c
            },
        );
    }
    let text: String = chars.iter().collect();
    let (s, e) = find_url_span(&text, col)?;
    Some((chars[s..e].iter().collect(), s, e))
}

/// open in the default browser, detached (xdg-open = QDesktopServices::openUrl)
fn open_url(url: &str) {
    use std::process::Stdio;
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// if the app enabled mouse reporting (vim mouse=a, htop, ...): report, return true
fn try_report_mouse(shared: &Arc<Shared>, m: &MouseEvent, g: &GridGeom) -> bool {
    let term = shared.term.lock();
    let mode = term.mode();
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return false;
    }
    let col = (m.x / g.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
    let row = (m.y / g.cell_h).clamp(0, term.grid().screen_lines() as i32 - 1);
    let base = match m.button {
        qt::mouse_button::LEFT => 0,
        qt::mouse_button::MIDDLE => 1,
        qt::mouse_button::RIGHT => 2,
        _ => 3, // no button (motion)
    };
    let b = base | mouse_mods(m.mods);
    let (code, press) = match m.kind {
        k if k == qt::mouse_kind::PRESS || k == qt::mouse_kind::DOUBLE_CLICK => (b, true),
        k if k == qt::mouse_kind::RELEASE => (b, false),
        k if k == qt::mouse_kind::MOVE => {
            if !mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) {
                return false;
            }
            if m.button == 0 && !mode.contains(TermMode::MOUSE_MOTION) {
                return false; // drag-only mode: ignore free movement
            }
            (b | 32, true)
        }
        _ => return false,
    };
    let bytes = mouse_report(&term, code, col, row, press);
    drop(term);
    let _ = shared.writer.lock().unwrap().write_all(&bytes);
    true
}

/// owned copy of everything render() needs from the grid: taken under a short
/// lock so the reader thread is never stalled by QPainter time (vtebench:
/// painting used to hold the FairMutex and throttle parsing)
struct CellSnap {
    point: Point,
    line: i32, // screen row (grid line + display_offset)
    cell: alacritty_terminal::term::cell::Cell,
}

struct GridSnap {
    cells: Vec<CellSnap>,
    cursor: Point,
    offset: i64,
    sel: Option<alacritty_terminal::selection::SelectionRange>,
}

fn snapshot_grid(term: &AppTerm) -> GridSnap {
    let offset = term.grid().display_offset() as i64;
    let sel = term.selection.as_ref().and_then(|s| s.to_range(term));
    let cursor = term.grid().cursor.point;
    let mut cells = Vec::with_capacity(term.grid().screen_lines() * term.grid().columns());
    for indexed in term.grid().display_iter() {
        let line = i64::from(indexed.point.line.0) + offset;
        if line < 0 {
            continue;
        }
        cells.push(CellSnap {
            point: indexed.point,
            line: line as i32,
            cell: indexed.cell.clone(),
        });
    }
    GridSnap {
        cells,
        cursor,
        offset,
        sel,
    }
}

/// text cursor presentation for one paint: `shape` from config, `focused` =
/// this pane has input focus. Unfocused panes draw a hollow block whatever the
/// configured shape (deepin-terminal focus hint); ponytail: no blink
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub shape: config::CursorShape,
    pub focused: bool,
}

/// cursor overlay rects, cell-local px (x, y, w, h)
fn cursor_rects(cur: Cursor, cw: i32, ch: i32) -> Vec<(i32, i32, i32, i32)> {
    use config::CursorShape::*;
    let t = (ch / 8).max(1); // stroke/line thickness
    match (cur.focused, cur.shape) {
        (true, Block) => vec![(0, 0, cw, ch)],
        (true, Beam) => vec![(0, 0, t, ch)],
        (true, Underline) => vec![(0, ch - t, cw, t)],
        (false, _) => vec![
            (0, 0, cw, t),
            (0, ch - t, cw, t),
            (0, 0, t, ch),
            (cw - t, 0, t, ch),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
fn render(
    snap: &GridSnap,
    p: &Painter,
    g: &GridGeom,
    fonts: &Fonts,
    w: i32,
    h: i32,
    y_off: i32,
    cur: Cursor,
) {
    let sc = scheme();
    p.set_font(&fonts.normal);
    // paint the whole viewport: default-bg cells must match the scheme, not the widget bg
    p.fill_rect(
        0,
        y_off,
        w,
        h - y_off,
        &color_q(Color::Named(NamedColor::Background), &sc),
    );
    let mut run = String::new();
    let mut run_line = -1i64;
    let mut run_col = 0usize;
    let mut run_st: Option<RunStyle> = None;
    let mut next_col = 0usize;
    let cursor = snap.cursor;
    let offset = snap.offset;
    let sel = snap.sel.as_ref();
    for cs in &snap.cells {
        let line = i64::from(cs.line);
        let col = cs.point.column.0;
        let cell = &cs.cell;
        use alacritty_terminal::term::cell::Flags;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue; // bg/cursor already painted by the wide char to our left
        }
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        let span = if wide { 2 } else { 1 };
        let (fg, bg) = cell_colors(cell);
        let st = RunStyle::of(cell, fg);
        // cursor on either cell of a wide char covers both
        let cur_col = cursor.column.0;
        let is_cursor = offset == 0
            && i64::from(cursor.line.0) == line
            && (cur_col == col || (wide && cur_col == col + 1));
        let selected = sel.is_some_and(|r| r.contains(cs.point));
        if is_cursor {
            let (o_r, o_g, o_b) = if sc.dark { (255, 255, 255) } else { (0, 0, 0) };
            let q = QColor::rgba(o_r, o_g, o_b, 110);
            let (cx, cy) = (col as i32 * g.cell_w, y_off + line as i32 * g.cell_h);
            for (rx, ry, rw, rh) in cursor_rects(cur, span * g.cell_w, g.cell_h) {
                p.fill_rect(cx + rx, cy + ry, rw, rh, &q);
            }
        } else if selected || !matches!(bg, Color::Named(NamedColor::Background)) {
            let (o_r, o_g, o_b) = if sc.dark { (255, 255, 255) } else { (0, 0, 0) };
            let q = if selected {
                QColor::rgba(o_r, o_g, o_b, 60)
            } else {
                color_q(bg, &sc)
            };
            p.fill_rect(
                col as i32 * g.cell_w,
                y_off + line as i32 * g.cell_h,
                span * g.cell_w,
                g.cell_h,
                &q,
            );
        }
        if cell.c == '\0' {
            continue; // wide-char padding: bg handled, no glyph
        }
        // spaces and HIDDEN (SGR 8) cells join the run as blanks: glyphs are
        // invisible either way, but decorations (strikeout/underline) must not
        // break at every space — alacritty draws them continuously
        let c = if cell.flags.contains(Flags::HIDDEN) {
            ' '
        } else {
            cell.c
        };
        if !c.is_ascii() {
            // fallback-font glyphs (powerline, CJK, ...) have advance != cell width:
            // draw them pinned to their cell or every glyph after them drifts
            flush_run(p, g, y_off, &mut run, run_line, run_col, run_st, &sc, fonts);
            run_line = -1;
            p.set_font(match (st.bold, st.italic) {
                (true, true) => &fonts.bold_italic,
                (true, false) => &fonts.bold,
                (false, true) => &fonts.italic,
                (false, false) => &fonts.normal,
            });
            p.set_pen_color(&color_q(fg, &sc));
            let mut buf = [0u8; 4];
            // fallback glyphs can exceed the cell (box-drawing, CJK): clip or rows overlap
            p.save();
            p.set_clip_rect(
                col as i32 * g.cell_w,
                y_off + line as i32 * g.cell_h,
                span * g.cell_w,
                g.cell_h,
            );
            p.draw_text_at(
                col as i32 * g.cell_w,
                y_off + line as i32 * g.cell_h + g.ascent,
                c.encode_utf8(&mut buf),
            );
            p.restore();
            continue;
        }
        if line != run_line || col != next_col || Some(st) != run_st {
            flush_run(p, g, y_off, &mut run, run_line, run_col, run_st, &sc, fonts);
            run_line = line;
            run_col = col;
            run_st = Some(st);
        }
        run.push(c);
        next_col = col + 1;
    }
    flush_run(p, g, y_off, &mut run, run_line, run_col, run_st, &sc, fonts);
}

/// visible grid as text (test assertions)
pub fn grid_text(term: &AppTerm) -> String {
    let offset = term.grid().display_offset() as i32;
    let mut out = String::new();
    for indexed in term.grid().display_iter() {
        if indexed.point.line.0 + offset >= 0 {
            out.push(indexed.cell.c);
        }
    }
    out
}

const SB_W: i32 = 14;
const MOD_MASK: i32 = qt::modifier::CONTROL | qt::modifier::SHIFT | qt::modifier::ALT;

// ---- tabs and split panes ----

/// one pane = one shell: terminal state + widget + scrollbar
pub struct Pane {
    pub id: u64,
    pub shared: Arc<Shared>,
    pub pid: u32,
    pub pw: PaintWidget,
    pub sb: ScrollBar,
    /// last layout rect in container coords (divider hit-testing from pane events)
    pub rect: Rc<Cell<(i32, i32, i32, i32)>>,
}

/// split tree; Leaf holds a pane id, panes live in Tab::panes.
/// Empty only transiently during tree surgery.
pub enum Node {
    Empty,
    Leaf(u64),
    /// vertical divider (纵向): a left, b right; otherwise a top, b bottom.
    /// ratio = share of space for `a` (draggable); id identifies the divider
    Split {
        id: u64,
        vertical: bool,
        ratio: Cell<f64>,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// divider thickness in px; hit area gets HIT_SLACK extra on each side
const DIV: i32 = 2;
const HIT_SLACK: i32 = 3;

/// child rects + divider band for a split occupying (x, y, w, h)
#[allow(clippy::type_complexity)]
fn split_geometry(
    vertical: bool,
    ratio: f64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> (
    (i32, i32, i32, i32),
    (i32, i32, i32, i32),
    (i32, i32, i32, i32),
) {
    if vertical {
        let aw = ((w - DIV).max(0) as f64 * ratio) as i32;
        (
            (x, y, aw, h),
            (x + aw + DIV, y, (w - DIV - aw).max(0), h),
            (x + aw, y, DIV, h),
        )
    } else {
        let ah = ((h - DIV).max(0) as f64 * ratio) as i32;
        (
            (x, y, w, ah),
            (x, y + ah + DIV, w, (h - DIV - ah).max(0)),
            (x, y + ah, w, DIV),
        )
    }
}

/// one tab = a container widget + a split tree of panes
pub struct Tab {
    pub tree: Node,
    pub panes: Vec<Pane>,
    pub container: QWidget,
    pub active_pane: Cell<u64>,
}

impl Tab {
    pub fn pane(&self, id: u64) -> &Pane {
        self.panes
            .iter()
            .find(|p| p.id == id)
            .expect("pane id in tree")
    }
    pub fn active(&self) -> &Pane {
        self.pane(self.active_pane.get())
    }
}

fn next_pane_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn first_leaf(node: &Node) -> Option<u64> {
    match node {
        Node::Empty => None,
        Node::Leaf(id) => Some(*id),
        Node::Split { a, .. } => first_leaf(a),
    }
}

/// swap leaf `id` for a subtree (split); false if not found
fn replace_leaf(node: &mut Node, id: u64, new: Node) -> bool {
    match node {
        Node::Leaf(i) => {
            if *i == id {
                *node = new;
                true
            } else {
                false
            }
        }
        Node::Split { a, b, .. } => {
            if matches!(a.as_ref(), Node::Leaf(i) if *i == id) {
                **a = new;
                true
            } else if matches!(b.as_ref(), Node::Leaf(i) if *i == id) {
                **b = new;
                true
            } else if rect_of(a, id, 0, 0, 0, 0).is_some() {
                replace_leaf(a, id, new)
            } else {
                replace_leaf(b, id, new)
            }
        }
        Node::Empty => false,
    }
}

/// remove leaf `id`, collapsing its parent split into the sibling.
/// Root-leaf case is the caller's (the whole tab goes away).
fn remove_leaf(node: &mut Node, id: u64) -> bool {
    let Node::Split { a, b, .. } = node else {
        return false;
    };
    if matches!(a.as_ref(), Node::Leaf(i) if *i == id) {
        let sib = std::mem::replace(b, Box::new(Node::Empty));
        *node = *sib;
        return true;
    }
    if matches!(b.as_ref(), Node::Leaf(i) if *i == id) {
        let sib = std::mem::replace(a, Box::new(Node::Empty));
        *node = *sib;
        return true;
    }
    remove_leaf(a, id) || remove_leaf(b, id)
}

/// pixel rect of leaf `id` within a w×h box (same math as layout_tree)
fn rect_of(node: &Node, id: u64, x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
    match node {
        Node::Leaf(i) => (*i == id).then_some((x, y, w, h)),
        Node::Split {
            vertical,
            ratio,
            a,
            b,
            ..
        } => {
            let (ra, rb, _) = split_geometry(*vertical, ratio.get(), x, y, w, h);
            rect_of(a, id, ra.0, ra.1, ra.2, ra.3)
                .or_else(|| rect_of(b, id, rb.0, rb.1, rb.2, rb.3))
        }
        Node::Empty => None,
    }
}

/// (split id, vertical, split rect, divider band) for every split, layout coords
#[allow(clippy::type_complexity)]
fn dividers_of(
    node: &Node,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    out: &mut Vec<(u64, bool, (i32, i32, i32, i32), (i32, i32, i32, i32))>,
) {
    if let Node::Split {
        id,
        vertical,
        ratio,
        a,
        b,
    } = node
    {
        let (ra, rb, band) = split_geometry(*vertical, ratio.get(), x, y, w, h);
        out.push((*id, *vertical, (x, y, w, h), band));
        dividers_of(a, ra.0, ra.1, ra.2, ra.3, out);
        dividers_of(b, rb.0, rb.1, rb.2, rb.3, out);
    }
}

/// (ratio cell, split rect, band) of divider `id`
#[allow(clippy::type_complexity)]
fn find_divider(
    node: &Node,
    id: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<(&Cell<f64>, bool, (i32, i32, i32, i32))> {
    match node {
        Node::Split {
            id: i,
            vertical,
            ratio,
            a,
            b,
        } => {
            let (ra, rb, _) = split_geometry(*vertical, ratio.get(), x, y, w, h);
            if *i == id {
                Some((ratio, *vertical, (x, y, w, h)))
            } else {
                find_divider(a, id, ra.0, ra.1, ra.2, ra.3)
                    .or_else(|| find_divider(b, id, rb.0, rb.1, rb.2, rb.3))
            }
        }
        _ => None,
    }
}

fn point_in(r: (i32, i32, i32, i32), x: i32, y: i32, slack: i32) -> bool {
    x >= r.0 - slack && x < r.0 + r.2 + slack && y >= r.1 - slack && y < r.1 + r.3 + slack
}

/// leaf pane ids in tree (visual) order: left-to-right, top-to-bottom
fn leaf_ids(node: &Node, out: &mut Vec<u64>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { a, b, .. } => {
            leaf_ids(a, out);
            leaf_ids(b, out);
        }
        Node::Empty => {}
    }
}

/// equal-share units along axis `vertical` in this subtree: a same-axis split
/// flattens into its children's units, anything else counts as one unit
/// equal-share units along axis `vertical` in this subtree: a same-axis split
/// flattens into its children's units, anything else (leaf or a split on the
/// other axis) is one unit — a vertical column inside a horizontal strip
/// occupies exactly one slot of that strip
fn axis_units(node: &Node, vertical: bool) -> usize {
    match node {
        Node::Split {
            vertical: v, a, b, ..
        } if *v == vertical => axis_units(a, vertical) + axis_units(b, vertical),
        Node::Empty => 0,
        _ => 1,
    }
}

/// reset ratios so every same-axis strip splits its space into equal units
/// (deepin-terminal: N splits along one axis -> each pane gets 1/N)
fn equalize_axis(node: &mut Node, vertical: bool) {
    let Node::Split {
        vertical: v,
        ratio,
        a,
        b,
        ..
    } = node
    else {
        return;
    };
    if *v == vertical {
        let total = axis_units(a, vertical) + axis_units(b, vertical);
        if total > 0 {
            ratio.set(axis_units(a, vertical) as f64 / total as f64);
        }
    }
    equalize_axis(a, vertical);
    equalize_axis(b, vertical);
}

/// orientation of the split node directly holding leaf `id`
fn parent_axis(node: &Node, id: u64) -> Option<bool> {
    match node {
        Node::Split { vertical, a, b, .. } => {
            if matches!(a.as_ref(), Node::Leaf(l) if *l == id)
                || matches!(b.as_ref(), Node::Leaf(l) if *l == id)
            {
                return Some(*vertical);
            }
            parent_axis(a, id).or_else(|| parent_axis(b, id))
        }
        _ => None,
    }
}

/// first leaf of the sibling subtree sharing `id`'s parent split: the focus
/// target when `id` closes (deepin-terminal WidgetTreeReverseFindTerm: first
/// remaining terminal in the same splitter, walking up the tree)
fn focus_fallback(node: &Node, id: u64) -> Option<u64> {
    match node {
        Node::Split { a, b, .. } => {
            if matches!(a.as_ref(), Node::Leaf(l) if *l == id) {
                return first_leaf(b);
            }
            if matches!(b.as_ref(), Node::Leaf(l) if *l == id) {
                return first_leaf(a);
            }
            focus_fallback(a, id).or_else(|| focus_fallback(b, id))
        }
        _ => None,
    }
}

/// nearest pane in direction (dx, dy) from `cur`: strictly past the edge on
/// the dominant axis, shortest center distance (deepin-terminal focusNavigation)
fn dir_target(rects: &[(u64, (i32, i32, i32, i32))], cur: u64, dx: i32, dy: i32) -> Option<u64> {
    let &(_, cr) = rects.iter().find(|(id, _)| *id == cur)?;
    let cc = (cr.0 + cr.2 / 2, cr.1 + cr.3 / 2);
    let mut best: Option<(i64, u64)> = None;
    for &(id, r) in rects {
        if id == cur {
            continue;
        }
        let c = (r.0 + r.2 / 2, r.1 + r.3 / 2);
        let (ddx, ddy) = (c.0 - cc.0, c.1 - cc.1);
        let ok = match (dx, dy) {
            (1, 0) => ddx > 0 && ddy.abs() < ddx,
            (-1, 0) => ddx < 0 && ddy.abs() < -ddx,
            (0, 1) => ddy > 0 && ddx.abs() < ddy,
            (0, -1) => ddy < 0 && ddx.abs() < -ddy,
            _ => false,
        };
        if !ok {
            continue;
        }
        let d2 = i64::from(ddx) * i64::from(ddx) + i64::from(ddy) * i64::from(ddy);
        if best.is_none_or(|(bd, _)| d2 < bd) {
            best = Some((d2, id));
        }
    }
    best.map(|(_, id)| id)
}

/// first configured binding matching (key, mods): the Ctrl/Shift/Alt mask must
/// match the binding exactly, same rule the key handler uses
pub fn match_action(cfg: &Config, key: i32, mods: i32) -> Option<config::Action> {
    cfg.key_bindings
        .iter()
        .find(|b| b.key_code() == Some(key) && mods & MOD_MASK == b.mod_mask())
        .map(|b| b.action)
}

/// splits are ratio-draggable; divider bands are painted by the root widget
fn layout_tree(tab: &Tab, node: &Node, x: i32, y: i32, w: i32, h: i32) {
    match node {
        Node::Empty => {}
        Node::Leaf(id) => {
            let p = tab.pane(*id);
            p.rect.set((x, y, w, h));
            p.pw.as_widget().move_to(x, y);
            p.pw.as_widget().resize(w.max(1), h.max(1));
        }
        Node::Split {
            vertical,
            ratio,
            a,
            b,
            ..
        } => {
            let (ra, rb, _) = split_geometry(*vertical, ratio.get(), x, y, w, h);
            layout_tree(tab, a, ra.0, ra.1, ra.2, ra.3);
            layout_tree(tab, b, rb.0, rb.1, rb.2, rb.3);
        }
    }
}

fn layout_tab(tab: &Tab) {
    let (w, h) = (tab.container.width(), tab.container.height());
    layout_tree(tab, &tab.tree, 0, 0, w, h);
}

/// app state shared by every handler (all Copy/Rc parts; GUI thread only)
#[derive(Clone)]
pub struct App {
    pub cfg: Config,
    pub tabs: Rc<RefCell<Vec<Tab>>>,
    pub active: Rc<Cell<usize>>,
    pub tabbar: DTabBar,
    pub root: PaintWidget,
    pub geom: Rc<GridGeom>,
    pub win: DMainWindow,
}

fn cwd_of_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// new tab/split inherits the focused shell's cwd (deepin-terminal behavior)
fn active_cwd(tabs: &Rc<RefCell<Vec<Tab>>>, active: &Rc<Cell<usize>>) -> Option<String> {
    cwd_of_pid(tabs.borrow()[active.get()].active().pid)
}

/// SIGHUP every shell in the tab; each reader thread's EOF poke ('q') removes
/// its pane, and the last pane removes the tab — one code path for "shell gone"
pub fn close_tab(tabs: &Rc<RefCell<Vec<Tab>>>, i: usize) {
    let pids: Vec<u32> = tabs.borrow()[i].panes.iter().map(|p| p.pid).collect();
    for pid in pids {
        if pid != 0 {
            unsafe { libc::kill(pid as i32, libc::SIGHUP) };
        }
    }
}

fn default_title(cfg: &Config) -> String {
    let path = cfg
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or("/bin/bash".into());
    path.rsplit('/').next().unwrap_or("shell").to_string()
}

/// shell for a new pane: config shell / $SHELL / bash
fn shell_cmd(cfg: &Config) -> CommandBuilder {
    let shell = cfg
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or("/bin/bash".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    cmd
}

/// term + pty + reader thread; the reader pokes the GUI over a socketpair,
/// one byte per event ('x' output, 'q' shell gone)
fn spawn_shell(
    cfg: &Config,
    cols: usize,
    lines: usize,
    cwd: Option<String>,
) -> (Arc<Shared>, u32, &'static mut UnixStream) {
    let title_slot = Arc::new(Mutex::new(None::<String>));
    let term = Term::new(
        term::Config {
            scrolling_history: cfg.scrollback,
            ..Default::default()
        },
        &Size { cols, lines },
        TitleListener {
            slot: title_slot.clone(),
        },
    );
    let shared = Arc::new(Shared {
        term: FairMutex::new(term),
        title: title_slot,
        title_armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        writer: Mutex::new(Box::new(std::io::sink()) as Box<dyn Write + Send>),
        master: Mutex::new(None),
    });
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: lines as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");
    let mut cmd = shell_cmd(cfg);
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    let mut child = pty.slave.spawn_command(cmd).expect("spawn shell failed");
    let pid = child.process_id().unwrap_or(0);
    *shared.writer.lock().unwrap() = pty.master.take_writer().expect("pty writer");

    let (mut gui_end, gui_read) = UnixStream::pair().expect("socketpair");
    gui_read.set_nonblocking(true).expect("nonblocking");
    let gui_read: &'static mut UnixStream = Box::leak(Box::new(gui_read));
    {
        let shared = shared.clone();
        let mut reader = pty.master.try_clone_reader().expect("pty reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            let mut processor = Processor::<StdSyncHandler>::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut term = shared.term.lock();
                            processor.advance(&mut *term, &buf[..n]);
                        }
                        if gui_end.write_all(b"x").is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = child.wait();
            let _ = gui_end.write_all(b"q");
        });
    }
    *shared.master.lock().unwrap() = Some(pty.master);
    drop(pty.slave); // keep no slave fd: master must see EOF when the shell exits
    (shared, pid, gui_read)
}

/// per-pane notifier: repaint on output, remove the pane on shell exit
fn make_notifier(gui_read: &'static mut UnixStream, app: &App, me: &Arc<Shared>) {
    let notifier = QSocketNotifier::new(gui_read.as_raw_fd());
    notifier.on_activated({
        let app = app.clone();
        let me = me.clone();
        move || {
            let mut drain = [0u8; 4096];
            let mut exited = false;
            while let Ok(n) = gui_read.read(&mut drain) {
                if n == 0 {
                    break;
                }
                exited |= drain[..n].contains(&b'q');
            }
            // locate (tab, pane) by shared identity
            let loc = {
                let ts = app.tabs.borrow();
                ts.iter().enumerate().find_map(|(ti, t)| {
                    t.panes
                        .iter()
                        .find(|p| Arc::ptr_eq(&p.shared, &me))
                        .map(|p| (ti, p.id))
                })
            };
            let Some((ti, pane_id)) = loc else { return };
            if exited {
                // fd stays "readable" at EOF: stop the notifier or the event
                // loop re-fires on it forever (100% CPU after the pane closes)
                notifier.set_enabled(false);
                let last_pane = app.tabs.borrow()[ti].panes.len() == 1;
                if last_pane {
                    // last pane: the whole tab goes (single code path for "shell gone")
                    // NB: tabbar.remove_tab/set_current_index emit currentChanged, which
                    // borrows `tabs` — never call them while holding a borrow (reentrancy)
                    {
                        let tab = app.tabs.borrow_mut().remove(ti);
                        tab.container.delete_later();
                    }
                    app.tabbar.remove_tab(ti as i32);
                    if app.tabs.borrow().is_empty() {
                        DApplication::quit();
                        return;
                    }
                    let next = app.active.get().min(app.tabs.borrow().len() - 1);
                    app.active.set(next);
                    app.tabbar.set_current_index(next as i32);
                    app.tabbar.as_widget().flush_layout();
                    return;
                }
                let focus_pw = {
                    let mut ts = app.tabs.borrow_mut();
                    let tab = &mut ts[ti];
                    // deepin-terminal closeSplit (WidgetTreeReverseFindTerm): focus
                    // the first remaining terminal of the same split group — not
                    // the global top-left leaf
                    let fallback = focus_fallback(&tab.tree, pane_id);
                    let axis = parent_axis(&tab.tree, pane_id);
                    remove_leaf(&mut tab.tree, pane_id);
                    if let Some(axis) = axis {
                        equalize_axis(&mut tab.tree, axis); // closing a third leaves halves
                    }
                    if let Some(pos) = tab.panes.iter().position(|p| p.id == pane_id) {
                        let p = tab.panes.remove(pos);
                        p.pw.as_widget().delete_later(); // takes the scrollbar child with it
                    }
                    let mut focus = None;
                    if tab.active_pane.get() == pane_id
                        && let Some(nid) = fallback
                    {
                        tab.active_pane.set(nid);
                        focus = Some(tab.pane(nid).pw);
                    }
                    layout_tab(tab);
                    focus
                };
                // set_focus fires Focus events synchronously; they borrow tabs
                if let Some(pw) = focus_pw {
                    pw.set_focus();
                }
                return;
            }
            // OSC title: coalesce like qtermwidget's 20ms title timer — rapid
            // reset->set bursts (prompt redraw after every command) collapse to
            // the latest value, so transient titles never reach the tab
            if me.title.lock().unwrap().is_some()
                && !me
                    .title_armed
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let me = me.clone();
                let app = app.clone();
                QTimer::single_shot(30, move || {
                    me.title_armed
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    let t = me.title.lock().unwrap().take();
                    if let Some(t) = t {
                        let ts = app.tabs.borrow();
                        if let Some((ti, tab)) = ts
                            .iter()
                            .enumerate()
                            .find(|(_, t)| t.panes.iter().any(|p| Arc::ptr_eq(&p.shared, &me)))
                        {
                            // tab label follows the focused pane's title
                            if tab.panes.iter().any(|p| Arc::ptr_eq(&p.shared, &me))
                                && tab.active_pane.get()
                                    == tab
                                        .panes
                                        .iter()
                                        .find(|p| Arc::ptr_eq(&p.shared, &me))
                                        .map(|p| p.id)
                                        .unwrap_or(0)
                                && app.tabbar.tab_text(ti as i32) != t
                            {
                                app.tabbar.set_tab_text(ti as i32, &t);
                                app.tabbar.as_widget().flush_layout();
                            }
                        }
                    }
                });
            }
            // repaint only if this pane is on screen
            let ts = app.tabs.borrow();
            if ti == app.active.get()
                && let Some(tab) = ts.get(ti)
                && let Some(p) = tab.panes.iter().find(|p| Arc::ptr_eq(&p.shared, &me))
            {
                p.pw.update();
            }
        }
    });
    notifier.leak();
}

fn copy_selection(shared: &Arc<Shared>) {
    let text = shared.term.lock().selection_to_string();
    if let Some(t) = text {
        Clipboard::set_text(&t);
    }
}

fn paste(shared: &Arc<Shared>) {
    let text = Clipboard::text();
    if text.is_empty() {
        return;
    }
    let bracketed = shared
        .term
        .lock()
        .mode()
        .contains(TermMode::BRACKETED_PASTE);
    let mut w = shared.writer.lock().unwrap();
    if bracketed {
        let _ = w.write_all(b"\x1b[200~");
    }
    let _ = w.write_all(text.as_bytes());
    if bracketed {
        let _ = w.write_all(b"\x1b[201~");
    }
}

/// right-click menu: only features deptty actually supports
pub fn context_menu(app: &App, pane_id: u64, at: &QWidget, x: i32, y: i32) {
    let menu = DMenu::new(at);
    let shared = {
        let ts = app.tabs.borrow();
        ts.iter()
            .flat_map(|t| t.panes.iter())
            .find(|p| p.id == pane_id)
            .map(|p| p.shared.clone())
    };
    let Some(shared) = shared else { return };
    let pid = {
        let ts = app.tabs.borrow();
        ts.iter()
            .flat_map(|t| t.panes.iter())
            .find(|p| p.id == pane_id)
            .map(|p| p.pid)
            .unwrap_or(0)
    };
    {
        let shared = shared.clone();
        menu.add_action(&t!("menu.copy"), move || copy_selection(&shared));
    }
    {
        let shared = shared.clone();
        menu.add_action(&t!("menu.paste"), move || paste(&shared));
    }
    menu.add_action(&t!("menu.open_in_file_manager"), move || {
        if let Some(dir) = cwd_of_pid(pid) {
            open_url(&dir); // xdg-open on a directory opens the file manager
        }
    });
    menu.add_separator();
    {
        let app = app.clone();
        menu.add_action(&t!("menu.split_horizontal"), move || {
            split_pane(&app, pane_id, false);
        });
    }
    {
        let app = app.clone();
        menu.add_action(&t!("menu.split_vertical"), move || {
            split_pane(&app, pane_id, true);
        });
    }
    menu.add_separator();
    {
        let app = app.clone();
        menu.add_action(&t!("menu.new_tab"), move || {
            let cwd = active_cwd(&app.tabs, &app.active);
            spawn_tab(&app, cwd);
        });
    }
    {
        let app = app.clone();
        menu.add_action(&t!("menu.close_workspace"), move || {
            let ti = {
                let ts = app.tabs.borrow();
                ts.iter()
                    .position(|t| t.panes.iter().any(|p| p.id == pane_id))
            };
            if let Some(ti) = ti {
                close_tab(&app.tabs, ti);
            }
        });
    }
    menu.popup(at, x, y);
}

/// paint + input widget for one pane; the handler closes over the pane's own shell
fn make_pane(
    app: &App,
    container: &QWidget,
    id: u64,
    shared: Arc<Shared>,
    pid: u32,
    gui_read: &'static mut UnixStream,
) -> Pane {
    let pw_slot = Rc::new(RefCell::new(None::<PaintWidget>));
    let sb_slot = Rc::new(RefCell::new(None::<ScrollBar>));
    let syncing_sb = Rc::new(Cell::new(false));
    // pane's layout rect in container coords; layout_tree writes, hover reads
    let pane_rect = Rc::new(Cell::new((0i32, 0i32, 0i32, 0i32)));
    let pw = PaintWidget::new(Some(container), {
        let app = app.clone();
        let geom = app.geom.clone();
        let fonts = Fonts::of(&app.cfg);
        let shared = shared.clone();
        let pw_slot = pw_slot.clone();
        let sb_slot = sb_slot.clone();
        let syncing_sb = syncing_sb.clone();
        let selecting = Rc::new(Cell::new(false));
        // hovered grid cell (screen line, col); the URL span is recomputed from
        // the grid snapshot every paint, so links typed or grown under a
        // stationary mouse underline in real time (deepin-terminal parity)
        let hover = Rc::new(Cell::new(None::<(i32, usize)>));
        // last mouse position + current cursor shape: Ctrl press/release
        // re-evaluates hover without a mouse move (deepin-terminal parity)
        let mpos = Rc::new(Cell::new((0i32, 0i32)));
        let cur_shape = Rc::new(Cell::new(qt::cursor::IBEAM));
        let update_hover = {
            let shared = shared.clone();
            let geom = geom.clone();
            let hover = hover.clone();
            let pw_slot = pw_slot.clone();
            let cur_shape = cur_shape.clone();
            move |x: i32, y: i32, ctrl: bool| {
                let (row, col, over) = {
                    let term = shared.term.lock();
                    let col = (x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
                    let row = (y / geom.cell_h).clamp(0, term.grid().screen_lines() as i32 - 1);
                    (row, col, url_at(&term, row, col).is_some())
                };
                // underline on any hover; clickable (pointing hand) only with Ctrl
                let shape = if over && ctrl {
                    qt::cursor::POINTING_HAND
                } else {
                    qt::cursor::IBEAM
                };
                if shape != cur_shape.get() {
                    cur_shape.set(shape);
                    if let Some(w) = &*pw_slot.borrow() {
                        w.as_widget().set_cursor(shape);
                    }
                }
                if hover.get() != Some((row, col)) {
                    hover.set(Some((row, col)));
                    if let Some(w) = &*pw_slot.borrow() {
                        w.update();
                    }
                }
            }
        };
        // click streak for double/triple click: (when, line, col, count)
        let streak = Rc::new(RefCell::new(None::<(std::time::Instant, i32, usize, u8)>));
        let pane_rect_h = pane_rect.clone();
        // active divider drag initiated from this pane's slack zone
        let pane_drag = Rc::new(Cell::new(None::<u64>));
        move |ev| match ev {
            PaintWidgetEvent::Paint(p, w, h) => {
                // short lock: snapshot the grid, then paint unlocked so the
                // reader thread never waits on QPainter
                let snap = {
                    let term = shared.term.lock();
                    snapshot_grid(&term)
                };
                let focused = {
                    let ts = app.tabs.borrow();
                    ts.iter()
                        .find(|t| t.panes.iter().any(|p| p.id == id))
                        .is_some_and(|t| t.active_pane.get() == id)
                };
                let cur = Cursor {
                    shape: app.cfg.cursor_shape,
                    focused,
                };
                render(&snap, &p, &geom, &fonts, w, h, 0, cur);
                // hovered link underline: span recomputed from this frame's
                // snapshot (mouse-over, deepin-terminal style)
                if let Some((line, col)) = hover.get() {
                    use alacritty_terminal::term::cell::Flags;
                    let mut chars: Vec<char> = Vec::new();
                    for cs in &snap.cells {
                        if cs.line != line {
                            continue;
                        }
                        let c = cs.cell.c;
                        chars.push(
                            if c == '\0' || cs.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                                ' '
                            } else {
                                c
                            },
                        );
                    }
                    let text: String = chars.iter().collect();
                    if let Some((s, e)) = find_url_span(&text, col) {
                        let lw = (geom.cell_h / 12).max(1);
                        let c = color_q(Color::Named(NamedColor::Foreground), &scheme());
                        p.fill_rect(
                            s as i32 * geom.cell_w,
                            line * geom.cell_h + geom.ascent + 1,
                            (e - s) as i32 * geom.cell_w,
                            lw,
                            &c,
                        );
                    }
                }
                if let Some(sb) = *sb_slot.borrow() {
                    let term = shared.term.lock();
                    sync_scrollbar(&term, sb, &syncing_sb);
                }
                // keep the IME candidate window glued to the cursor
                let cur = snap.cursor;
                let row = cur.line.0 + snap.offset as i32;
                if row >= 0
                    && let Some(w) = &*pw_slot.borrow()
                {
                    w.set_ime_cursor_rect(
                        cur.column.0 as i32 * geom.cell_w,
                        row * geom.cell_h,
                        geom.cell_w,
                        geom.cell_h,
                    );
                }
            }
            // Ctrl press/release over a link toggles the clickable cursor even
            // without a mouse move
            PaintWidgetEvent::Key(k) if k.key == qt::key::CONTROL => {
                let (x, y) = mpos.get();
                update_hover(x, y, k.press);
            }
            PaintWidgetEvent::Key(k) if k.press => {
                // configurable bindings first (alacritty-style [[key_binding]])
                let hit = match_action(&app.cfg, k.key, k.mods);
                if let Some(action) = hit {
                    use config::Action::*;
                    match action {
                        Copy => copy_selection(&shared),
                        Paste => paste(&shared),
                        NewTab => {
                            let cwd = cwd_of_pid(pid);
                            spawn_tab(&app, cwd);
                        }
                        CloseTab => {
                            let ti = {
                                let ts = app.tabs.borrow();
                                ts.iter().position(|t| t.panes.iter().any(|p| p.id == id))
                            };
                            if let Some(ti) = ti {
                                close_tab(&app.tabs, ti);
                            }
                        }
                        NextTab => switch_tab(&app, app.active.get() + 1),
                        PrevTab => switch_tab(&app, app.active.get().wrapping_sub(1)),
                        SplitHorizontal => {
                            split_pane(&app, id, false);
                        }
                        SplitVertical => {
                            split_pane(&app, id, true);
                        }
                        NextPane => cycle_pane(&app, 1),
                        PrevPane => cycle_pane(&app, -1),
                        FocusPaneUp => focus_pane_dir(&app, 0, -1),
                        FocusPaneDown => focus_pane_dir(&app, 0, 1),
                        FocusPaneLeft => focus_pane_dir(&app, -1, 0),
                        FocusPaneRight => focus_pane_dir(&app, 1, 0),
                    }
                    return;
                }
                let ctrl = k.mods & qt::modifier::CONTROL != 0;
                if k.key == qt::key::ESCAPE || (!ctrl && !k.text.is_empty()) {
                    // typing clears the selection, like every other terminal
                    shared.term.lock().selection = None;
                }
                // typing jumps back to the prompt; modifier-only presses
                // (Ctrl/Shift for a shortcut) produce no bytes and must not scroll
                let app_cursor = shared.term.lock().mode().contains(TermMode::APP_CURSOR);
                if let Some(bytes) = key_bytes(&k, app_cursor) {
                    shared.term.lock().scroll_display(Scroll::Bottom);
                    let _ = shared.writer.lock().unwrap().write_all(&bytes);
                }
            }
            PaintWidgetEvent::Ime { commit, .. } if !commit.is_empty() => {
                let _ = shared.writer.lock().unwrap().write_all(commit.as_bytes());
            }
            PaintWidgetEvent::Mouse(m) => {
                if try_report_mouse(&shared, &m, &geom) {
                    return; // app owns the mouse (vim/htop): no local selection
                }
                match m.kind {
                    k if k == qt::mouse_kind::PRESS && m.button == qt::mouse_button::RIGHT => {
                        if let Some(w) = &*pw_slot.borrow() {
                            context_menu(&app, id, &w.as_widget(), m.x, m.y);
                        }
                    }
                    k if k == qt::mouse_kind::PRESS && m.button == qt::mouse_button::LEFT => {
                        // divider slack zone: drag the divider instead of selecting
                        let (px, py, _, _) = pane_rect_h.get();
                        let host = pw_slot.borrow().map(|w| w.as_widget());
                        if let Some(host) = host
                            && divider_mouse(
                                &app,
                                &pane_drag,
                                &host,
                                m.kind,
                                m.button,
                                px + m.x,
                                py + m.y,
                            )
                        {
                            return;
                        }
                        if let Some(w) = &*pw_slot.borrow() {
                            w.set_focus();
                        }
                        // Ctrl+click on a link opens it in the browser (deepin-terminal)
                        if m.mods & qt::modifier::CONTROL != 0 {
                            let term = shared.term.lock();
                            let col = (m.x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1)
                                as usize;
                            let row =
                                (m.y / geom.cell_h).clamp(0, term.grid().screen_lines() as i32 - 1);
                            if let Some((url, _, _)) = url_at(&term, row, col) {
                                drop(term);
                                open_url(&url);
                                return;
                            }
                        }
                        selecting.set(true);
                        let mut term = shared.term.lock();
                        let (pt, side) = mouse_point(m.x, m.y, &geom, &term);
                        let now = std::time::Instant::now();
                        // rapid re-click on the same cell: even streaks (3rd, 5th... click
                        // arrives as PRESS) select whole lines, cycling with word select
                        let prev = *streak.borrow();
                        let (ty, count) = match prev {
                            Some((t, l, c, n))
                                if now.duration_since(t)
                                    < std::time::Duration::from_millis(500)
                                    && l == pt.line.0
                                    && c == pt.column.0
                                    && n % 2 == 0 =>
                            {
                                (SelectionType::Lines, n + 1)
                            }
                            _ => (SelectionType::Simple, 1),
                        };
                        *streak.borrow_mut() = Some((now, pt.line.0, pt.column.0, count));
                        term.selection = Some(Selection::new(ty, pt, side));
                        drop(term);
                        if let Some(w) = &*pw_slot.borrow() {
                            w.update();
                        }
                    }
                    k if k == qt::mouse_kind::DOUBLE_CLICK
                        && m.button == qt::mouse_button::LEFT =>
                    {
                        // double-click: semantic (word) selection; dragging keeps expanding by word
                        selecting.set(true);
                        let mut term = shared.term.lock();
                        let (pt, _) = mouse_point(m.x, m.y, &geom, &term);
                        let now = std::time::Instant::now();
                        let count = match *streak.borrow() {
                            Some((t, l, c, n))
                                if now.duration_since(t)
                                    < std::time::Duration::from_millis(500)
                                    && l == pt.line.0
                                    && c == pt.column.0 =>
                            {
                                n + 1
                            }
                            _ => 2,
                        };
                        *streak.borrow_mut() = Some((now, pt.line.0, pt.column.0, count));
                        term.selection =
                            Some(Selection::new(SelectionType::Semantic, pt, Side::Left));
                        drop(term);
                        if let Some(w) = &*pw_slot.borrow() {
                            w.update();
                        }
                    }
                    k if k == qt::mouse_kind::MOVE && pane_drag.get().is_some() => {
                        let (px, py, _, _) = pane_rect_h.get();
                        divider_drag(&app, pane_drag.get().unwrap(), px + m.x, py + m.y);
                    }
                    k if k == qt::mouse_kind::MOVE && selecting.get() => {
                        {
                            let mut term = shared.term.lock();
                            let (pt, side) = mouse_point(m.x, m.y, &geom, &term);
                            if let Some(sel) = &mut term.selection {
                                sel.update(pt, side);
                            }
                        }
                        if let Some(w) = &*pw_slot.borrow() {
                            w.update();
                        }
                    }
                    k if k == qt::mouse_kind::MOVE => {
                        mpos.set((m.x, m.y));
                        // near a divider (incl. the pane-side slack): resize cursor;
                        // the band itself is container territory, cursor inherited from root
                        let (px, py, _, _) = pane_rect_h.get();
                        match divider_at(&app, px + m.x, py + m.y) {
                            Some((_, vertical)) => {
                                let shape = if vertical {
                                    qt::cursor::SIZE_HOR // vertical line drags ↔
                                } else {
                                    qt::cursor::SIZE_VER // horizontal line drags ↕
                                };
                                if shape != cur_shape.get() {
                                    cur_shape.set(shape);
                                    if let Some(w) = &*pw_slot.borrow() {
                                        w.as_widget().set_cursor(shape);
                                    }
                                }
                            }
                            // link hover: underline on mouse-over (no Ctrl needed), like
                            // deepin-terminal; Ctrl is only required for the click
                            None => update_hover(m.x, m.y, m.mods & qt::modifier::CONTROL != 0),
                        }
                    }
                    k if k == qt::mouse_kind::RELEASE && m.button == qt::mouse_button::LEFT => {
                        if pane_drag.replace(None).is_some() {
                            return; // divider drag ended; hover fixes the cursor
                        }
                        selecting.set(false);
                        let mut term = shared.term.lock();
                        if term.selection.as_ref().is_some_and(Selection::is_empty) {
                            term.selection = None; // bare click: no selection
                            if let Some(w) = &*pw_slot.borrow() {
                                w.update();
                            }
                        }
                    }
                    _ => {}
                }
            }
            PaintWidgetEvent::Wheel { dy, x, y, mods } => {
                {
                    let term = shared.term.lock();
                    if term.mode().intersects(TermMode::MOUSE_MODE) {
                        // wheel as button 64/65 presses, one per notch
                        let col =
                            (x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
                        let row = (y / geom.cell_h).clamp(0, term.grid().screen_lines() as i32 - 1);
                        let b = 64 + mouse_mods(mods) + i32::from(dy < 0);
                        let n = (dy.abs() / 120).max(1);
                        let mut out = Vec::new();
                        for _ in 0..n {
                            out.extend_from_slice(&mouse_report(&term, b, col, row, true));
                        }
                        drop(term);
                        let _ = shared.writer.lock().unwrap().write_all(&out);
                        return;
                    }
                }
                let lines = dy * 3 / 120; // wheel up (dy>0) -> scroll into history
                if lines != 0 {
                    shared.term.lock().scroll_display(Scroll::Delta(lines));
                    if let Some(w) = &*pw_slot.borrow() {
                        w.update();
                    }
                }
            }
            PaintWidgetEvent::Focus(true) => {
                // this pane is now its tab's focus target (splits, menu actions)
                let pws: Vec<PaintWidget> = {
                    let ts = app.tabs.borrow();
                    ts.iter()
                        .find(|t| t.panes.iter().any(|p| p.id == id))
                        .map(|t| {
                            t.active_pane.set(id);
                            t.panes.iter().map(|p| p.pw).collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                // solid/hollow cursor swaps on focus change: repaint every pane
                for pw in pws {
                    pw.update();
                }
            }
            PaintWidgetEvent::Focus(false) => {
                // clicking the titlebar moves focus there; take it back next turn —
                // unless a sibling pane took focus (it became the tab's active pane)
                // or a modal/popup (context menu) is active. Only refocus a pane of
                // the CURRENT tab: hidden tabs' panes must never grab focus back,
                // or two tabs ping-pong focus every 0ms and the event loop wedges.
                // ponytail: refocus blindly otherwise; revisit when dialogs land
                let app = app.clone();
                let pw_slot = pw_slot.clone();
                QTimer::single_shot(0, move || {
                    if DApplication::popup_active() {
                        return;
                    }
                    let still_active = {
                        let ts = app.tabs.borrow();
                        ts.iter()
                            .enumerate()
                            .find(|(_, t)| t.panes.iter().any(|p| p.id == id))
                            .is_some_and(|(ti, t)| {
                                ti == app.active.get() && t.active_pane.get() == id
                            })
                    };
                    if !still_active {
                        return;
                    }
                    if let Some(w) = &*pw_slot.borrow() {
                        w.set_focus();
                    }
                });
            }
            PaintWidgetEvent::Resize { w, h } => {
                if let Some(sb) = *sb_slot.borrow() {
                    sb.as_widget().move_to(w - SB_W, 0);
                    sb.as_widget().resize(SB_W, h);
                }
                let cols = (w / geom.cell_w).max(1) as usize;
                let lines = (h / geom.cell_h).max(1) as usize;
                shared.term.lock().resize(Size { cols, lines });
                if let Some(m) = &*shared.master.lock().unwrap() {
                    let _ = m.resize(PtySize {
                        rows: lines as u16,
                        cols: cols as u16,
                        pixel_width: w as u16,
                        pixel_height: h as u16,
                    });
                }
            }
            _ => {}
        }
    });
    *pw_slot.borrow_mut() = Some(pw);
    // deepin-terminal look: I-beam over the grid (arrow is the default)
    pw.as_widget().set_cursor(qt::cursor::IBEAM);

    // DTK scrollbar, overlaid on the right edge of the pane
    let sb = ScrollBar::new(&pw.as_widget());
    // child widgets inherit the parent's I-beam; scrollbar wants the arrow
    sb.as_widget().set_cursor(qt::cursor::ARROW);
    sb.as_widget().show();
    *sb_slot.borrow_mut() = Some(sb);
    sb.on_value_changed({
        let shared = shared.clone();
        let syncing = syncing_sb.clone();
        let pw_slot = pw_slot.clone();
        move |v| {
            if syncing.get() {
                return; // programmatic sync, not user drag
            }
            let mut term = shared.term.lock();
            let target = term.grid().history_size() as i32 - v;
            let delta = target - term.grid().display_offset() as i32;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
                drop(term);
                if let Some(w) = &*pw_slot.borrow() {
                    w.update();
                }
            }
        }
    });

    make_notifier(gui_read, app, &shared);

    Pane {
        id,
        shared,
        pid,
        pw,
        sb,
        rect: pane_rect,
    }
}

/// new tab: container widget + one pane; the tabbar's currentChanged handler
/// makes it visible and focuses the pane
pub fn spawn_tab(app: &App, cwd: Option<String>) {
    let (w, h) = (app.root.width(), app.root.height());
    let cols = (w / app.geom.cell_w).max(1) as usize;
    let lines = (h / app.geom.cell_h).max(1) as usize;
    let (shared, pid, gui_read) = spawn_shell(&app.cfg, cols, lines, cwd);
    // tab container: a transparent PaintWidget covering the root; pane widgets
    // sit on top, the divider bands between them are the container's exposed
    // pixels (root's divider color shows through). The container handles band
    // hover + drag; panes handle their own slack zones.
    let cslot = Rc::new(RefCell::new(None::<PaintWidget>));
    let container = PaintWidget::new(Some(&app.root.as_widget()), {
        let app = app.clone();
        let cslot = cslot.clone();
        let drag = Rc::new(Cell::new(None::<u64>));
        move |ev| {
            let Some(me) = *cslot.borrow() else { return };
            let mw = me.as_widget();
            if let PaintWidgetEvent::Mouse(m) = ev {
                if divider_mouse(&app, &drag, &mw, m.kind, m.button, m.x, m.y) {
                    return;
                }
                if m.kind == qt::mouse_kind::MOVE {
                    let shape = match divider_at(&app, m.x, m.y) {
                        Some((_, true)) => qt::cursor::SIZE_HOR,
                        Some((_, false)) => qt::cursor::SIZE_VER,
                        None => qt::cursor::ARROW,
                    };
                    mw.set_cursor(shape);
                }
            }
        }
    });
    *cslot.borrow_mut() = Some(container);
    // PaintWidget defaults to StrongFocus; the container must never eat keys
    container.set_focus_policy(qt::focus::NO_FOCUS);
    let container = container.as_widget();
    container.resize(w.max(1), h.max(1));
    let id = next_pane_id();
    let pane = make_pane(app, &container, id, shared, pid, gui_read);
    let title = default_title(&app.cfg);
    let tab = Tab {
        tree: Node::Leaf(id),
        panes: vec![pane],
        container,
        active_pane: Cell::new(id),
    };
    app.tabs.borrow_mut().push(tab);
    let i = app.tabbar.add_tab(&title);
    // deepin-terminal caps tab item width (their tabbar.cpp: min 110 / max 450px);
    // long titles elide instead of stretching the tab. Height unconstrained.
    const QWIDGETSIZE_MAX: i32 = (1 << 24) - 1;
    app.tabbar.set_tab_minimum_size(i, &QSize::new(110, 0));
    app.tabbar
        .set_tab_maximum_size(i, &QSize::new(450, QWIDGETSIZE_MAX));
    app.tabbar.set_current_index(i); // fires currentChanged: shows + focuses
    app.tabbar.as_widget().flush_layout();
}

/// divider color: DPalette::Highlight, same as deepin-terminal's split line
/// (their termwidgetpage.cpp setSplitStyle); ours is just a bit thicker
fn divider_color() -> QColor {
    let (r, g, b) = DApplication::palette_highlight_rgb();
    QColor::rgb(i32::from(r), i32::from(g), i32::from(b))
}

/// divider under (x, y) on the active tab: (id, vertical) for cursor/drag
fn divider_at(app: &App, x: i32, y: i32) -> Option<(u64, bool)> {
    let ts = app.tabs.borrow();
    let tab = &ts[app.active.get()];
    let mut divs = Vec::new();
    dividers_of(
        &tab.tree,
        0,
        0,
        tab.container.width(),
        tab.container.height(),
        &mut divs,
    );
    divs.iter()
        .find(|(_, _, _, band)| point_in(*band, x, y, HIT_SLACK))
        .map(|(id, vertical, _, _)| (*id, *vertical))
}

/// drag divider `id` on the active tab to the mouse position
fn divider_drag(app: &App, id: u64, x: i32, y: i32) {
    let ts = app.tabs.borrow();
    let tab = &ts[app.active.get()];
    let (w, h) = (tab.container.width(), tab.container.height());
    let Some((ratio, vertical, (sx, sy, sw, sh))) = find_divider(&tab.tree, id, 0, 0, w, h) else {
        return;
    };
    let r = if vertical {
        (x - sx) as f64 / (sw - DIV).max(1) as f64
    } else {
        (y - sy) as f64 / (sh - DIV).max(1) as f64
    };
    ratio.set(r.clamp(0.05, 0.95));
    layout_tab(tab);
}

/// divider drag mouse handling shared by pane widgets (slack zone) and the tab
/// container (the band itself). (x, y) in container coords; `w` hosts the cursor.
/// Returns true when the event was consumed.
fn divider_mouse(
    app: &App,
    drag: &Cell<Option<u64>>,
    w: &QWidget,
    kind: i32,
    button: i32,
    x: i32,
    y: i32,
) -> bool {
    if kind == qt::mouse_kind::PRESS && button == qt::mouse_button::LEFT {
        if let Some((id, vertical)) = divider_at(app, x, y) {
            drag.set(Some(id));
            // keep the resize cursor for the whole drag (the widget grabs the mouse)
            w.set_cursor(if vertical {
                qt::cursor::SIZE_HOR // vertical line drags <->
            } else {
                qt::cursor::SIZE_VER // horizontal line drags up/down
            });
            divider_drag(app, id, x, y);
            return true;
        }
        return false;
    }
    if kind == qt::mouse_kind::MOVE {
        if let Some(id) = drag.get() {
            divider_drag(app, id, x, y);
            return true;
        }
        return false;
    }
    if kind == qt::mouse_kind::RELEASE && drag.get().is_some() {
        drag.set(None);
        w.set_cursor(qt::cursor::ARROW);
        // container/pane grabbed the focus on press; give the keys back
        let ts = app.tabs.borrow();
        ts[app.active.get()].active().pw.set_focus();
        return true;
    }
    false
}

/// split the pane in two; `vertical` = vertical divider, panes left/right (纵向).
/// Returns the new pane's shared state. 50/50; the tree rebalances on close.
pub fn split_pane(app: &App, pane_id: u64, vertical: bool) -> Option<Arc<Shared>> {
    // phase 1: geometry + cwd under a short borrow; the new pane halves the old rect
    let (ti, cols, lines, cwd) = {
        let ts = app.tabs.borrow();
        let ti = ts
            .iter()
            .position(|t| t.panes.iter().any(|p| p.id == pane_id))?;
        let tab = &ts[ti];
        let (cw, ch) = (tab.container.width(), tab.container.height());
        let (_, _, w, h) = rect_of(&tab.tree, pane_id, 0, 0, cw, ch)?;
        let (nw, nh) = if vertical { (w / 2, h) } else { (w, h / 2) };
        if nw < app.geom.cell_w || nh < app.geom.cell_h {
            return None; // no room for even one cell
        }
        (
            ti,
            (nw / app.geom.cell_w).max(1) as usize,
            (nh / app.geom.cell_h).max(1) as usize,
            cwd_of_pid(tab.pane(pane_id).pid),
        )
    };
    let (shared, pid, gui_read) = spawn_shell(&app.cfg, cols, lines, cwd);
    let new_id = next_pane_id();
    // phase 2: widget creation fires events (focus/resize) that borrow tabs
    // themselves — never hold a tabs borrow across it
    let container = app.tabs.borrow()[ti].container;
    let pane = make_pane(app, &container, new_id, shared.clone(), pid, gui_read);
    {
        let mut ts = app.tabs.borrow_mut();
        let tab = &mut ts[ti];
        replace_leaf(
            &mut tab.tree,
            pane_id,
            Node::Split {
                id: next_pane_id(),
                vertical,
                ratio: Cell::new(0.5),
                a: Box::new(Node::Leaf(pane_id)),
                b: Box::new(Node::Leaf(new_id)),
            },
        );
        tab.panes.push(pane);
        tab.active_pane.set(new_id);
        // deepin-terminal: N same-axis splits share the space equally (thirds, ...)
        equalize_axis(&mut tab.tree, vertical);
        layout_tab(tab);
    }
    let pw = app.tabs.borrow()[ti].pane(new_id).pw;
    pw.as_widget().show();
    pw.set_focus();
    Some(shared)
}

/// focus the next/previous pane in leaf order (konsole Ctrl+Tab view cycling)
pub fn cycle_pane(app: &App, step: i32) {
    let pw = {
        let ts = app.tabs.borrow();
        let tab = &ts[app.active.get()];
        let mut ids = Vec::new();
        leaf_ids(&tab.tree, &mut ids);
        if ids.len() < 2 {
            return;
        }
        let cur = ids
            .iter()
            .position(|i| *i == tab.active_pane.get())
            .unwrap_or(0);
        let next = ids[(cur as i32 + step).rem_euclid(ids.len() as i32) as usize];
        tab.pane(next).pw
    };
    // set_focus fires Focus events synchronously; they borrow tabs (reentrancy)
    pw.set_focus();
}

/// focus the nearest pane in direction (dx, dy); no-op when none lies that way
pub fn focus_pane_dir(app: &App, dx: i32, dy: i32) {
    let pw = {
        let ts = app.tabs.borrow();
        let tab = &ts[app.active.get()];
        let (w, h) = (tab.container.width(), tab.container.height());
        let mut ids = Vec::new();
        leaf_ids(&tab.tree, &mut ids);
        let rects: Vec<_> = ids
            .iter()
            .filter_map(|id| rect_of(&tab.tree, *id, 0, 0, w, h).map(|r| (*id, r)))
            .collect();
        dir_target(&rects, tab.active_pane.get(), dx, dy).map(|id| tab.pane(id).pw)
    };
    if let Some(pw) = pw {
        pw.set_focus();
    }
}

pub fn switch_tab(app: &App, i: usize) {
    if app.tabs.borrow().is_empty() {
        return;
    }
    let n = app.tabs.borrow().len();
    app.tabbar.set_current_index(i.rem_euclid(n) as i32); // currentChanged does the rest
}

/// system locale -> rust-i18n locale id ("zh-CN" / "en")
fn detect_locale() -> &'static str {
    let lang = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    if lang.starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

/// build the whole app: window, tabbar, first shell. Returns the application
/// object (for exec) and the shared app state (handlers, tests).
pub fn boot(cfg: Config) -> (DApplication, App) {
    rust_i18n::set_locale(detect_locale());
    let dapp = DApplication::new("deptty");
    DApplication::set_application_display_name("deptty");
    dapp.load_translator(); // DTK titlebar menu (about/theme) follows the locale
    let win = DMainWindow::new();
    win.set_window_title("deptty");

    // font + grid geometry (terminal needs a monospace font)
    let font = make_font(&cfg);
    let (cell_w, cell_h, ascent) = font.metrics();
    let geom = Rc::new(GridGeom {
        cell_w,
        cell_h,
        ascent,
    });

    let tabs = Rc::new(RefCell::new(Vec::<Tab>::new()));
    let active = Rc::new(Cell::new(0usize));
    let tabbar = DTabBar::new();

    let app = App {
        cfg: cfg.clone(),
        tabs,
        active,
        tabbar,
        root: PaintWidget::new(None, |_| {}), // placeholder, replaced below
        geom,
        win,
    };

    // root widget: covers the window; its Resize drives every tab's layout.
    // Pane widgets sit on top; the gaps between them are the split dividers,
    // which the root paints and hit-tests for drag-resize.
    // root widget: covers the window; its Resize drives every tab's layout.
    // Tab containers sit on top; root paint is the divider color showing
    // through the bands between panes.
    let root = PaintWidget::new(None, {
        let app = app.clone();
        move |ev| {
            match ev {
                PaintWidgetEvent::Paint(p, w, h) => {
                    // tab containers cover everything but the divider bands
                    p.fill_rect(0, 0, w, h, &divider_color());
                }
                PaintWidgetEvent::Resize { w, h } => {
                    // ponytail: one tiny state.toml write per resize frame, no debounce;
                    // add a QTimer debounce if disk churn ever matters
                    config::State {
                        window_width: Some(app.win.width()),
                        window_height: Some(app.win.height()),
                    }
                    .save();
                    for tab in app.tabs.borrow().iter() {
                        tab.container.resize(w, h);
                        layout_tab(tab);
                    }
                }
                _ => {}
            }
        }
    });

    let app = App { root, ..app };

    // DTK tab bar, embedded in the window titlebar (deepin-terminal style)
    tabbar.set_tabs_closable(true);
    tabbar.set_visible_add_button(true);
    tabbar.set_expanding(false); // deepin-terminal look: compact, content-sized tabs
    tabbar.set_document_mode(true);
    tabbar.set_movable(true); // drag to reorder; tabMoved syncs the tabs vec
    tabbar.set_dragable(false); // no DTK QDrag drag-out (nested exec loop, eats keys)
    tabbar.as_widget().install_tab_label_style(); // deepin-terminal TermTabStyle equivalent
    tabbar.connect_signal_i32("currentChanged(int)", {
        let app = app.clone();
        move |i| {
            if i < 0 || i as usize >= app.tabs.borrow().len() {
                return;
            }
            let new = i as usize;
            // don't trust `active` as the old index: during a tab drag Qt may
            // deliver currentChanged before tabMoved reorders the vec. Just
            // re-align visibility with the bar's current index, every time.
            {
                let ts = app.tabs.borrow();
                for (j, t) in ts.iter().enumerate() {
                    if j != new {
                        t.container.hide();
                    }
                }
                let tab = &ts[new];
                tab.container.show();
                // hidden containers miss resize events; force the layout now
                tab.container.resize(app.root.width(), app.root.height());
                layout_tab(tab);
            }
            app.active.set(new);
            let pw = {
                let ts = app.tabs.borrow();
                ts[new].active().pw
            };
            pw.set_focus();
        }
    });
    tabbar.connect_signal_i32("tabCloseRequested(int)", {
        let app = app.clone();
        move |i| {
            if i >= 0 && (i as usize) < app.tabs.borrow().len() {
                close_tab(&app.tabs, i as usize);
            }
        }
    });
    tabbar.connect_signal_i32_i32("tabMoved(int,int)", {
        let app = app.clone();
        move |from, to| {
            let (from, to) = (from as usize, to as usize);
            {
                let mut ts = app.tabs.borrow_mut();
                if from >= ts.len() || to >= ts.len() || from == to {
                    return;
                }
                let tab = ts.remove(from);
                ts.insert(to, tab);
                // the moved tab keeps its widgets; only the vec order mirrors the bar.
                // fix the active index to track the same tab
                let a = app.active.get();
                let new_active = if a == from {
                    to
                } else if from < a && to >= a {
                    a - 1
                } else if from > a && to <= a {
                    a + 1
                } else {
                    a
                };
                app.active.set(new_active);
            }
            // currentChanged may have fired pre-reorder with a stale index;
            // re-align container visibility with the post-move order
            let pw = {
                let ts = app.tabs.borrow();
                let cur = app.active.get();
                for (j, t) in ts.iter().enumerate() {
                    if j != cur {
                        t.container.hide();
                    }
                }
                let tab = &ts[cur];
                tab.container.show();
                tab.container.resize(app.root.width(), app.root.height());
                layout_tab(tab);
                tab.active().pw
            };
            pw.set_focus();
        }
    });
    tabbar.connect_signal("tabAddRequested()", {
        let app = app.clone();
        move || {
            let cwd = active_cwd(&app.tabs, &app.active);
            spawn_tab(&app, cwd);
        }
    });

    win.set_central_widget(&root.as_widget());
    // remember window size (deepin-terminal window_width/height, konsole state rc)
    let st = config::State::load();
    win.resize(
        st.window_width.unwrap_or(80 * cell_w),
        st.window_height.unwrap_or(24 * cell_h),
    );

    let icon = QIcon::from_theme("deepin-terminal");
    win.set_window_icon(&icon);
    win.as_widget().set_titlebar_icon(&icon);
    win.as_widget().titlebar_set_tabbar(&tabbar.as_widget());
    win.show();

    // spawn the first shell only now: show() delivered the final widget size, so the
    // PTY opens at the real grid size and the shell never gets a startup SIGWINCH
    // (a WINCH after the prompt is drawn makes p10k erase+redraw it = startup flash)
    // dde-file-manager "open in terminal here" sets our process cwd; portable-pty
    // defaults the shell to $HOME unless we pass it through explicitly
    let cwd = work_dir_arg().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });
    spawn_tab(&app, cwd);
    root.leak();

    (dapp, app)
}

/// entry point: config, app, event loop
pub fn main_run() -> i32 {
    let cfg = Config::load();
    let (dapp, _app) = boot(cfg);
    dapp.exec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dtk::qt::{key, modifier};

    fn kev(k: i32, mods: i32, text: &str) -> KeyEvent {
        KeyEvent {
            key: k,
            mods,
            text: text.into(),
            press: true,
            autorepeat: false,
        }
    }

    #[test]
    fn modifiers() {
        assert_eq!(
            key_bytes(&kev(key::LEFT, 0, ""), false),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::LEFT, modifier::ALT, ""), false),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::RIGHT, modifier::ALT | modifier::SHIFT, ""), false),
            Some(b"\x1b[1;4C".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::BACKSPACE, 0, ""), false),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::BACKSPACE, modifier::ALT, ""), false),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(i32::from(b'L'), modifier::CONTROL, "\x0c"), false),
            Some(vec![0x0c])
        );
        assert_eq!(
            key_bytes(&kev(i32::from(b'F'), modifier::ALT, "f"), false),
            Some(b"\x1bf".to_vec())
        );
    }

    #[test]
    fn app_cursor_keys() {
        // DECCKM on: plain arrows go SS3; modified arrows keep CSI
        assert_eq!(
            key_bytes(&kev(key::UP, 0, ""), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::DOWN, 0, ""), true),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::UP, 0, ""), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::UP, modifier::SHIFT, ""), true),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn shift_tab_is_csi_z() {
        // xterm: Shift+Tab (Backtab) is always plain CSI Z, never mod-encoded
        assert_eq!(
            key_bytes(&kev(key::BACKTAB, modifier::SHIFT, ""), false),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn url_spans() {
        let t = "see https://example.com/a?b=1 end";
        assert_eq!(find_url_span(t, 5), Some((4, 29)));
        assert_eq!(find_url_span(t, 4), Some((4, 29)));
        assert_eq!(find_url_span(t, 29), None); // trailing space: off the link
        assert_eq!(find_url_span(t, 0), None);
        assert_eq!(find_url_span("no link at all", 3), None);
        assert_eq!(find_url_span("http://a.b", 0), Some((0, 10)));
        assert_eq!(find_url_span("(https://x.y)", 2), Some((1, 12))); // stops at ')'
        assert_eq!(
            find_url_span("see http://x and https://y.z", 20),
            Some((17, 28))
        );
    }

    #[test]
    fn menu_strings_localized() {
        rust_i18n::set_locale("zh-CN");
        assert_eq!(t!("menu.copy"), "复制");
        assert_eq!(t!("menu.split_horizontal"), "横向分屏");
        rust_i18n::set_locale("en");
        assert_eq!(t!("menu.copy"), "Copy");
        assert_eq!(t!("menu.split_vertical"), "Split Vertically");
    }

    #[test]
    fn cursor_shapes() {
        use config::CursorShape::*;
        let solid = |shape| {
            cursor_rects(
                Cursor {
                    shape,
                    focused: true,
                },
                10,
                20,
            )
        };
        let hollow = |shape| {
            cursor_rects(
                Cursor {
                    shape,
                    focused: false,
                },
                10,
                20,
            )
        };
        assert_eq!(solid(Block), vec![(0, 0, 10, 20)]);
        // beam: thin bar on the left edge; underline: thin bar at the bottom
        assert_eq!(solid(Beam), vec![(0, 0, 2, 20)]);
        assert_eq!(solid(Underline), vec![(0, 18, 10, 2)]);
        // unfocused pane: hollow block outline, whatever the configured shape
        for shape in [Block, Beam, Underline] {
            assert_eq!(
                hollow(shape),
                vec![(0, 0, 10, 2), (0, 18, 10, 2), (0, 0, 2, 20), (8, 0, 2, 20)]
            );
        }
    }

    #[test]
    fn pane_key_matching() {
        let cfg = Config::default();
        // konsole view cycling: Ctrl+Tab / Ctrl+Shift+Tab (Qt: Shift+Tab = Backtab)
        assert_eq!(
            match_action(&cfg, key::TAB, modifier::CONTROL),
            Some(config::Action::NextPane)
        );
        assert_eq!(
            match_action(&cfg, key::BACKTAB, modifier::CONTROL | modifier::SHIFT),
            Some(config::Action::PrevPane)
        );
        // plain / shift-only tab stays with the shell (\t / CSI Z)
        assert_eq!(match_action(&cfg, key::TAB, 0), None);
        assert_eq!(match_action(&cfg, key::BACKTAB, modifier::SHIFT), None);
    }

    #[test]
    fn equalize_shares() {
        // two same-axis splits -> equal thirds (deepin-terminal)
        let split = |vertical, a, b| Node::Split {
            id: 0,
            vertical,
            ratio: Cell::new(0.5),
            a: Box::new(a),
            b: Box::new(b),
        };
        let mut t = Node::Leaf(1);
        assert!(replace_leaf(
            &mut t,
            1,
            split(true, Node::Leaf(1), Node::Leaf(2))
        ));
        equalize_axis(&mut t, true);
        assert!(replace_leaf(
            &mut t,
            2,
            split(true, Node::Leaf(2), Node::Leaf(3))
        ));
        equalize_axis(&mut t, true);
        // 99px wide, DIV=2 per split: ~33px each
        let w = |id| rect_of(&t, id, 0, 0, 99, 40).unwrap().2;
        let (w1, w2, w3) = (w(1), w(2), w(3));
        assert!(
            (w1 - w2).abs() <= 1 && (w2 - w3).abs() <= 1,
            "{w1} {w2} {w3} not thirds"
        );
        // closing the middle pane rebalances the rest to halves
        let axis = parent_axis(&t, 2);
        assert_eq!(axis, Some(true));
        assert!(remove_leaf(&mut t, 2));
        equalize_axis(&mut t, axis.unwrap());
        assert_eq!(rect_of(&t, 1, 0, 0, 99, 40).unwrap().2, 48);
        assert_eq!(rect_of(&t, 3, 0, 0, 99, 40).unwrap().2, 49);
        // a mixed-axis subtree counts as one unit of the other axis
        assert!(replace_leaf(
            &mut t,
            3,
            split(false, Node::Leaf(3), Node::Leaf(4))
        ));
        equalize_axis(&mut t, false);
        let h = |id| rect_of(&t, id, 0, 0, 99, 40).unwrap().3;
        assert!((h(3) - h(4)).abs() <= 1, "{} {} not halves", h(3), h(4));
    }

    #[test]
    fn close_focus_fallback() {
        // A | (B over C): closing C falls back to its split sibling B
        // (deepin-terminal WidgetTreeReverseFindTerm), not top-left A
        let split = |vertical, a, b| Node::Split {
            id: 0,
            vertical,
            ratio: Cell::new(0.5),
            a: Box::new(a),
            b: Box::new(b),
        };
        let t = split(
            true,
            Node::Leaf(1),
            split(false, Node::Leaf(2), Node::Leaf(3)),
        );
        assert_eq!(focus_fallback(&t, 3), Some(2));
        assert_eq!(focus_fallback(&t, 2), Some(3));
        assert_eq!(focus_fallback(&t, 1), Some(2)); // sibling subtree's first leaf
        assert_eq!(focus_fallback(&t, 99), None);
    }

    #[test]
    fn directional_focus() {
        // A | B over C
        let rects = vec![
            (1u64, (0, 0, 50, 40)),
            (2u64, (50, 0, 50, 20)),
            (3u64, (50, 20, 50, 20)),
        ];
        assert_eq!(dir_target(&rects, 1, 1, 0), Some(2)); // right: B/C tie, first wins
        assert_eq!(dir_target(&rects, 1, -1, 0), None); // nothing left of A
        assert_eq!(dir_target(&rects, 2, 0, 1), Some(3));
        assert_eq!(dir_target(&rects, 3, 0, -1), Some(2));
        assert_eq!(dir_target(&rects, 3, -1, 0), Some(1));
        assert_eq!(dir_target(&rects, 2, 1, 0), None);
    }

    #[test]
    fn mixed_axis_equalize_keeps_strip_shares() {
        // h-split, v-split on the bottom, h-split on the bottom-right: the top
        // pane must keep its half — a vertical subtree is one slot in the
        // horizontal strip, not zero (a zero slot made the top pane take the
        // whole window)
        let split = |vertical, a, b| Node::Split {
            id: 0,
            vertical,
            ratio: Cell::new(0.5),
            a: Box::new(a),
            b: Box::new(b),
        };
        let mut t = Node::Leaf(1);
        assert!(replace_leaf(&mut t, 1, split(false, Node::Leaf(1), Node::Leaf(2))));
        equalize_axis(&mut t, false);
        assert!(replace_leaf(&mut t, 2, split(true, Node::Leaf(2), Node::Leaf(3))));
        equalize_axis(&mut t, true);
        assert!(replace_leaf(&mut t, 3, split(false, Node::Leaf(3), Node::Leaf(4))));
        equalize_axis(&mut t, false);
        // 99x80, DIV=2: top pane keeps half the height; the bottom-right
        // column splits horizontally into halves
        let r = |t: &Node, id: u64| rect_of(t, id, 0, 0, 99, 80).unwrap();
        assert_eq!(r(&t, 1).3, 39, "top pane must keep half the window");
        assert_eq!(r(&t, 2).3, 39, "bottom-left column keeps the other half");
        assert!((r(&t, 3).3 - r(&t, 4).3).abs() <= 1, "bottom-right splits horizontally");
        // the same strip re-equalizes when the top pane is split again:
        // three horizontal slots, each about a third of the height
        assert!(replace_leaf(&mut t, 1, split(false, Node::Leaf(1), Node::Leaf(5))));
        equalize_axis(&mut t, false);
        for id in [1, 5, 2] {
            let h = r(&t, id).3;
            assert!((h - 26).abs() <= 1, "pane {id} height {h} not a third");
        }
    }

    #[test]
    fn split_tree_surgery() {
        // replace + remove keep the tree balanced
        let split = |vertical, a, b| Node::Split {
            id: 0,
            vertical,
            ratio: Cell::new(0.5),
            a: Box::new(a),
            b: Box::new(b),
        };
        let mut t = Node::Leaf(1);
        assert!(replace_leaf(
            &mut t,
            1,
            split(true, Node::Leaf(1), Node::Leaf(2))
        ));
        assert!(replace_leaf(
            &mut t,
            2,
            split(false, Node::Leaf(2), Node::Leaf(3))
        ));
        assert!(!replace_leaf(&mut t, 99, Node::Empty));
        // layout math: 100x40, DIV=2 gap; left/right 49px each, then the right
        // half top/bottom 19px each
        assert_eq!(rect_of(&t, 1, 0, 0, 100, 40), Some((0, 0, 49, 40)));
        assert_eq!(rect_of(&t, 2, 0, 0, 100, 40), Some((51, 0, 49, 19)));
        assert_eq!(rect_of(&t, 3, 0, 0, 100, 40), Some((51, 21, 49, 19)));
        assert_eq!(first_leaf(&t), Some(1));
        // divider band of the root split sits between the halves
        let mut divs = Vec::new();
        dividers_of(&t, 0, 0, 100, 40, &mut divs);
        assert_eq!(divs.len(), 2);
        assert_eq!(divs[0].3, (49, 0, 2, 40));
        assert!(point_in(divs[0].3, 50, 20, HIT_SLACK));
        assert!(!point_in(divs[0].3, 10, 20, HIT_SLACK));
        // removing a leaf collapses the parent split into the sibling
        assert!(remove_leaf(&mut t, 3));
        assert!(matches!(t, Node::Split { vertical: true, .. }));
        assert!(remove_leaf(&mut t, 1));
        assert!(matches!(t, Node::Leaf(2)));
        assert!(!remove_leaf(&mut t, 2)); // root leaf: caller's business
    }
}
