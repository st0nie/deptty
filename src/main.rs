//! deptty: deepin-terminal rewritten in Rust on DTK6.
//! Terminal core: alacritty_terminal (VT parsing + screen grid) + portable-pty (PTY).
//! Rendering: QPainter cell grid, same approach as qtermwidget's TerminalDisplay.
mod config;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use config::Config;
use dtk::*;
use dtk::widgets::DTabBar;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

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
struct TitleListener {
    slot: Arc<Mutex<Option<String>>>,
}

impl alacritty_terminal::event::EventListener for TitleListener {
    fn send_event(&self, ev: alacritty_terminal::event::Event) {
        use alacritty_terminal::event::Event;
        // empty/reset titles are ignored on purpose: shells emit reset-then-set
        // around every prompt, applying the empty one flickers the tab label
        if let Event::Title(t) = ev {
            if !t.is_empty() {
                *self.slot.lock().unwrap() = Some(t);
            }
        }
    }
}

type AppTerm = Term<TitleListener>;

/// shared terminal state; reader thread feeds it, GUI thread renders it
struct Shared {
    term: FairMutex<AppTerm>,
    /// pending OSC title (reader writes); GUI applies it after a coalesce window
    title: Arc<Mutex<Option<String>>>,
    /// qtermwidget-style title debounce: only one apply timer in flight
    title_armed: Arc<std::sync::atomic::AtomicBool>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
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

struct GridGeom {
    cell_w: i32,
    cell_h: i32,
    ascent: i32,
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
        Self { normal, bold, italic, bold_italic }
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
        let lc = st.uline.map(|c| color_q(c, sc)).unwrap_or_else(|| color_q(st.fg, sc));
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
    let side = if x % g.cell_w < g.cell_w / 2 { Side::Left } else { Side::Right };
    (Point::new(Line(row - term.grid().display_offset() as i32), Column(col)), side)
}

/// SGR (1006) or X10 mouse report; x,y are 1-based screen cells.
/// b: button + mods(shift 4, alt 8, ctrl 16) + 32 motion + 64 wheel; release = 'm'/button 3
fn mouse_mods(mods: i32) -> i32 {
    (if mods & qt::modifier::SHIFT != 0 { 4 } else { 0 })
        | (if mods & qt::modifier::ALT != 0 { 8 } else { 0 })
        | (if mods & qt::modifier::CONTROL != 0 { 16 } else { 0 })
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

/// screen row of a mouse event, accounting for the tab bar strip
fn mouse_row(y: i32, g: &GridGeom) -> i32 {
    (y - TAB_H) / g.cell_h
}

/// char-index span of the http(s):// URL covering char index `col`, if any
fn find_url_span(text: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let delim = |c: char| {
        matches!(c, ' ' | '\t' | '"' | '\'' | '<' | '>' | '`' | '|' | '(' | ')' | '[' | ']' | '{' | '}')
    };
    let starts = |i: usize, pat: &str| {
        chars[i..].iter().take(pat.len()).copied().eq(pat.chars())
    };
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
        chars.push(if c == '\0' || indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { c });
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
    let row = mouse_row(m.y, g).clamp(0, term.grid().screen_lines() as i32 - 1);
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
    GridSnap { cells, cursor, offset, sel }
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
) {
    let sc = scheme();
    p.set_font(&fonts.normal);
    // paint the whole viewport: default-bg cells must match the scheme, not the widget bg
    p.fill_rect(0, y_off, w, h - y_off, &color_q(Color::Named(NamedColor::Background), &sc));
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
        if is_cursor || selected || !matches!(bg, Color::Named(NamedColor::Background)) {
            // ponytail: block cursor = translucent overlay, no blink
            let (o_r, o_g, o_b) = if sc.dark { (255, 255, 255) } else { (0, 0, 0) };
            let q = if is_cursor {
                QColor::rgba(o_r, o_g, o_b, 110)
            } else if selected {
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
        let c = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
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
            p.set_clip_rect(col as i32 * g.cell_w, y_off + line as i32 * g.cell_h, span * g.cell_w, g.cell_h);
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

/// visible grid as text (smoke assertions)
fn grid_text(term: &AppTerm) -> String {
    let offset = term.grid().display_offset() as i32;
    let mut out = String::new();
    for indexed in term.grid().display_iter() {
        if indexed.point.line.0 + offset >= 0 {
            out.push(indexed.cell.c);
        }
    }
    out
}


const TAB_H: i32 = 0; // tabs live in the titlebar; content uses the full widget
const SB_W: i32 = 14;
const MOD_MASK: i32 = qt::modifier::CONTROL | qt::modifier::SHIFT | qt::modifier::ALT;

/// one tab = one shell: terminal state + the shell's pid (cwd inheritance, SIGHUP on close)
struct Tab {
    shared: Arc<Shared>,
    pid: u32,
}

fn active_shared(tabs: &Rc<RefCell<Vec<Tab>>>, active: &Rc<Cell<usize>>) -> Arc<Shared> {
    tabs.borrow()[active.get()].shared.clone()
}

/// SIGHUP the shell; the reader thread's EOF poke ('q') removes the tab from the UI
fn close_tab(tabs: &Rc<RefCell<Vec<Tab>>>, i: usize) {
    let pid = tabs.borrow()[i].pid;
    if pid != 0 {
        unsafe { libc::kill(pid as i32, libc::SIGHUP) };
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

/// shell for a new tab: config shell / $SHELL / bash
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

#[allow(clippy::too_many_arguments)]
fn spawn_tab(
    cfg: &Config,
    tabs: &Rc<RefCell<Vec<Tab>>>,
    active: &Rc<Cell<usize>>,
    tabbar: DTabBar,
    view: &Rc<RefCell<Option<QWidget>>>,
    cols: usize,
    lines: usize,
    cwd: Option<String>,
) {
    let title_slot = Arc::new(Mutex::new(None::<String>));
    let term = Term::new(
        term::Config {
            scrolling_history: cfg.scrollback,
            ..Default::default()
        },
        &Size { cols, lines },
        TitleListener { slot: title_slot.clone() },
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

    // reader thread -> advance the terminal, poke the GUI over a per-tab socketpair
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

    // tab label: shell basename until the app sets a title (OSC 0/2)
    let title = default_title(cfg);

    // per-tab notifier: repaint on output, remove the tab on shell exit
    let notifier = QSocketNotifier::new(gui_read.as_raw_fd());
    notifier.on_activated({
        let tabs = tabs.clone();
        let active = active.clone();
        let view = view.clone();
        let me = shared.clone();
        move || {
            let mut drain = [0u8; 4096];
            let mut exited = false;
            while let Ok(n) = gui_read.read(&mut drain) {
                if n == 0 {
                    break;
                }
                exited |= drain[..n].contains(&b'q');
            }
            if exited {
                // fd stays "readable" at EOF: stop the notifier or the event
                // loop re-fires on it forever (100% CPU after the tab closes)
                notifier.set_enabled(false);
                // NB: tabbar.remove_tab/set_current_index emit currentChanged, which
                // borrows `tabs` — never call them while holding a borrow (reentrancy)
                let idx = tabs.borrow().iter().position(|t| Arc::ptr_eq(&t.shared, &me));
                if let Some(i) = idx {
                    tabs.borrow_mut().remove(i);
                    tabbar.remove_tab(i as i32);
                    if tabs.borrow().is_empty() {
                        DApplication::quit();
                        return;
                    }
                    let next = active.get().min(tabs.borrow().len() - 1);
                    active.set(next);
                    tabbar.set_current_index(next as i32);
                    tabbar.as_widget().flush_layout();
                }
                return;
            }
            // OSC title: coalesce like qtermwidget's 20ms title timer — rapid
            // reset->set bursts (prompt redraw after every command) collapse to
            // the latest value, so transient titles never reach the tab
            if me.title.lock().unwrap().is_some()
                && !me.title_armed.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let me = me.clone();
                let tabs = tabs.clone();
                QTimer::single_shot(30, move || {
                    me.title_armed.store(false, std::sync::atomic::Ordering::SeqCst);
                    let t = me.title.lock().unwrap().take();
                    if let Some(t) = t {
                        let ts = tabs.borrow();
                        if let Some(i) = ts.iter().position(|t2| Arc::ptr_eq(&t2.shared, &me)) {
                            if tabbar.tab_text(i as i32) != t {
                                tabbar.set_tab_text(i as i32, &t);
                                tabbar.as_widget().flush_layout();
                            }
                        }
                    }
                });
            }
            let ts = tabs.borrow();
            if let Some(i) = ts.iter().position(|t| Arc::ptr_eq(&t.shared, &me)) {
                // repaint only if this tab is on screen
                if i == active.get() {
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
            }
        }
    });
    notifier.leak();

    let i = tabbar.add_tab(&title);
    // deepin-terminal caps tab item width (their tabbar.cpp: min 110 / max 450px);
    // long titles elide instead of stretching the tab. Height unconstrained.
    const QWIDGETSIZE_MAX: i32 = (1 << 24) - 1;
    tabbar.set_tab_minimum_size(i, &QSize::new(110, 0));
    tabbar.set_tab_maximum_size(i, &QSize::new(450, QWIDGETSIZE_MAX));
    tabbar.set_current_index(i);
    tabbar.as_widget().flush_layout();
    tabs.borrow_mut().push(Tab { shared, pid });
    active.set(tabs.borrow().len() - 1);
    // repaint immediately only for the first tab (fresh window); for later tabs a
    // repaint now would draw one blank frame before the shell's first output —
    // keep the old pixels until the reader thread's first poke instead
    if tabs.borrow().len() == 1 {
        if let Some(w) = &*view.borrow() {
            w.update();
        }
    }
}

/// current grid size in cells, from the active tab
fn grid_cells(tabs: &Rc<RefCell<Vec<Tab>>>, active: &Rc<Cell<usize>>) -> (usize, usize) {
    let ts = tabs.borrow();
    let t = ts[active.get()].shared.term.lock();
    (t.grid().columns(), t.grid().screen_lines())
}

/// new tab inherits the active shell's cwd (deepin-terminal behavior)
fn active_cwd(tabs: &Rc<RefCell<Vec<Tab>>>, active: &Rc<Cell<usize>>) -> Option<String> {
    let pid = tabs.borrow()[active.get()].pid;
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn switch_tab(tabs: &Rc<RefCell<Vec<Tab>>>, active: &Rc<Cell<usize>>, tabbar: DTabBar, i: usize) {
    if tabs.borrow().is_empty() {
        return;
    }
    let n = tabs.borrow().len();
    let i = i.rem_euclid(n);
    active.set(i);
    tabbar.set_current_index(i as i32);
}

fn main() {
    let mut cfg = Config::load();
    let smoke = std::env::args().any(|a| a == "--smoke");
    if smoke {
        // deterministic smoke: bash has no precmd/title tricks
        cfg.shell = Some("/bin/bash".into());
    }

    let app = DApplication::new("deptty");
    DApplication::set_application_display_name("deptty");
    app.load_translator(); // DTK titlebar menu (about/theme) follows the locale
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

    // paint + input; the slots let handlers use widgets that only exist later
    let view = Rc::new(std::cell::RefCell::new(None::<QWidget>));
    let sb_slot = Rc::new(std::cell::RefCell::new(None::<ScrollBar>));
    let syncing_sb = Rc::new(std::cell::Cell::new(false));
    let tb_slot = Rc::new(std::cell::RefCell::new(None::<DTabBar>));
    // last grid size from Resize events; the first tab spawns after show() with it
    let cells = Rc::new(std::cell::Cell::new((80usize, 24usize)));
    let pw = PaintWidget::new(None, {
        let geom = geom.clone();
        let cfg = cfg.clone();
        let fonts = Fonts::of(&cfg);
        let view = view.clone();
        let sb_slot = sb_slot.clone();
        let syncing_sb = syncing_sb.clone();
        let tabs = tabs.clone();
        let active = active.clone();
        let tb_slot = tb_slot.clone();
        let cells = cells.clone();
        let selecting = Rc::new(std::cell::Cell::new(false));
        // hovered grid cell (screen line, col); the URL span is recomputed from
        // the grid snapshot every paint, so links typed or grown under a
        // stationary mouse underline in real time (deepin-terminal parity)
        let hover = Rc::new(std::cell::Cell::new(None::<(i32, usize)>));
        // last mouse position + current cursor shape: Ctrl press/release
        // re-evaluates hover without a mouse move (deepin-terminal parity)
        let mpos = Rc::new(std::cell::Cell::new((0i32, 0i32)));
        let cur_shape = Rc::new(std::cell::Cell::new(qt::cursor::IBEAM));
        let update_hover = {
            let tabs = tabs.clone();
            let active = active.clone();
            let geom = geom.clone();
            let hover = hover.clone();
            let view = view.clone();
            let cur_shape = cur_shape.clone();
            move |x: i32, y: i32, ctrl: bool| {
                let shared = active_shared(&tabs, &active);
                let (row, col, over) = {
                    let term = shared.term.lock();
                    let col = (x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
                    let row = mouse_row(y, &geom).clamp(0, term.grid().screen_lines() as i32 - 1);
                    (row, col, url_at(&term, row, col).is_some())
                };
                // underline on any hover; clickable (pointing hand) only with Ctrl
                let shape = if over && ctrl { qt::cursor::POINTING_HAND } else { qt::cursor::IBEAM };
                if shape != cur_shape.get() {
                    cur_shape.set(shape);
                    if let Some(w) = &*view.borrow() {
                        w.set_cursor(shape);
                    }
                }
                if hover.get() != Some((row, col)) {
                    hover.set(Some((row, col)));
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
            }
        };
        // click streak for double/triple click: (when, line, col, count)
        let streak = Rc::new(std::cell::RefCell::new(
            None::<(std::time::Instant, i32, usize, u8)>,
        ));
        move |ev| match ev {
            PaintWidgetEvent::Paint(p, w, h) => {
                let shared = active_shared(&tabs, &active);
                // short lock: snapshot the grid, then paint unlocked so the
                // reader thread never waits on QPainter
                let snap = {
                    let term = shared.term.lock();
                    snapshot_grid(&term)
                };
                render(&snap, &p, &geom, &fonts, w, h, TAB_H);
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
                        chars.push(if c == '\0' || cs.cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { c });
                    }
                    let text: String = chars.iter().collect();
                    if let Some((s, e)) = find_url_span(&text, col) {
                        let lw = (geom.cell_h / 12).max(1);
                        let c = color_q(Color::Named(NamedColor::Foreground), &scheme());
                        p.fill_rect(
                            s as i32 * geom.cell_w,
                            TAB_H + line * geom.cell_h + geom.ascent + 1,
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
                if row >= 0 {
                    if let Some(w) = &*view.borrow() {
                        w.set_ime_cursor_rect(
                            cur.column.0 as i32 * geom.cell_w,
                            TAB_H + row * geom.cell_h,
                            geom.cell_w,
                            geom.cell_h,
                        );
                    }
                }
            }
            // Ctrl press/release over a link toggles the clickable cursor even
            // without a mouse move
            PaintWidgetEvent::Key(k) if k.key == qt::key::CONTROL => {
                let (x, y) = mpos.get();
                update_hover(x, y, k.press);
            }
            PaintWidgetEvent::Key(k) if k.press => {
                let shared = active_shared(&tabs, &active);
                // configurable bindings first (alacritty-style [[key_binding]])
                let hit = cfg
                    .key_bindings
                    .iter()
                    .find(|b| b.key_code() == Some(k.key) && k.mods & MOD_MASK == b.mod_mask())
                    .map(|b| b.action);
                if let Some(action) = hit {
                    use config::Action::*;
                    match action {
                        Copy => {
                            let text = shared.term.lock().selection_to_string();
                            if let Some(t) = text {
                                Clipboard::set_text(&t);
                            }
                        }
                        Paste => {
                            let text = Clipboard::text();
                            if text.is_empty() {
                                return;
                            }
                            let bracketed =
                                shared.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
                            let mut w = shared.writer.lock().unwrap();
                            if bracketed {
                                let _ = w.write_all(b"\x1b[200~");
                            }
                            let _ = w.write_all(text.as_bytes());
                            if bracketed {
                                let _ = w.write_all(b"\x1b[201~");
                            }
                        }
                        NewTab => {
                            let cwd = active_cwd(&tabs, &active);
                            let (cols, lines) = grid_cells(&tabs, &active);
                            let tb = tb_slot.borrow().expect("tabbar");
                            spawn_tab(&cfg, &tabs, &active, tb, &view, cols, lines, cwd);
                        }
                        CloseTab => close_tab(&tabs, active.get()),
                        NextTab => {
                            let tb = tb_slot.borrow().expect("tabbar");
                            switch_tab(&tabs, &active, tb, active.get() + 1);
                            if let Some(w) = &*view.borrow() {
                                w.update();
                            }
                        }
                        PrevTab => {
                            let tb = tb_slot.borrow().expect("tabbar");
                            switch_tab(&tabs, &active, tb, active.get().wrapping_sub(1));
                            if let Some(w) = &*view.borrow() {
                                w.update();
                            }
                        }
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
                let shared = active_shared(&tabs, &active);
                let _ = shared.writer.lock().unwrap().write_all(commit.as_bytes());
            }
            PaintWidgetEvent::Mouse(m) => {
                let shared = active_shared(&tabs, &active);
                if try_report_mouse(&shared, &m, &geom) {
                    return; // app owns the mouse (vim/htop): no local selection
                }
                match m.kind {
                k if k == qt::mouse_kind::PRESS && m.button == qt::mouse_button::LEFT => {
                    // Ctrl+click on a link opens it in the browser (deepin-terminal)
                    if m.mods & qt::modifier::CONTROL != 0 {
                        let shared = active_shared(&tabs, &active);
                        let term = shared.term.lock();
                        let col = (m.x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
                        let row = mouse_row(m.y, &geom).clamp(0, term.grid().screen_lines() as i32 - 1);
                        if let Some((url, _, _)) = url_at(&term, row, col) {
                            drop(term);
                            open_url(&url);
                            return;
                        }
                    }
                    selecting.set(true);
                    let shared = active_shared(&tabs, &active);
                    let mut term = shared.term.lock();
                    let (pt, side) = mouse_point(m.x, m.y - TAB_H, &geom, &term);
                    let now = std::time::Instant::now();
                    // rapid re-click on the same cell: even streaks (3rd, 5th... click
                    // arrives as PRESS) select whole lines, cycling with word select
                    let prev = *streak.borrow();
                    let (ty, count) = match prev {
                        Some((t, l, c, n))
                            if now.duration_since(t) < std::time::Duration::from_millis(500)
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
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
                k if k == qt::mouse_kind::DOUBLE_CLICK && m.button == qt::mouse_button::LEFT => {
                    // double-click: semantic (word) selection; dragging keeps expanding by word
                    selecting.set(true);
                    let shared = active_shared(&tabs, &active);
                    let mut term = shared.term.lock();
                    let (pt, _) = mouse_point(m.x, m.y - TAB_H, &geom, &term);
                    let now = std::time::Instant::now();
                    let count = match *streak.borrow() {
                        Some((t, l, c, n))
                            if now.duration_since(t) < std::time::Duration::from_millis(500)
                                && l == pt.line.0
                                && c == pt.column.0 =>
                        {
                            n + 1
                        }
                        _ => 2,
                    };
                    *streak.borrow_mut() = Some((now, pt.line.0, pt.column.0, count));
                    term.selection = Some(Selection::new(SelectionType::Semantic, pt, Side::Left));
                    drop(term);
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
                k if k == qt::mouse_kind::MOVE && selecting.get() => {
                    {
                        let shared = active_shared(&tabs, &active);
                        let mut term = shared.term.lock();
                        let (pt, side) = mouse_point(m.x, m.y - TAB_H, &geom, &term);
                        if let Some(sel) = &mut term.selection {
                            sel.update(pt, side);
                        }
                    }
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
                k if k == qt::mouse_kind::MOVE => {
                    // link hover: underline on mouse-over (no Ctrl needed), like
                    // deepin-terminal; Ctrl is only required for the click
                    mpos.set((m.x, m.y));
                    update_hover(m.x, m.y, m.mods & qt::modifier::CONTROL != 0);
                }
                k if k == qt::mouse_kind::RELEASE && m.button == qt::mouse_button::LEFT => {
                    selecting.set(false);
                    let shared = active_shared(&tabs, &active);
                    let mut term = shared.term.lock();
                    if term.selection.as_ref().is_some_and(Selection::is_empty) {
                        term.selection = None; // bare click: no selection
                        if let Some(w) = &*view.borrow() {
                            w.update();
                        }
                    }
                }
                _ => {}
                }
            },
            PaintWidgetEvent::Wheel { dy, x, y, mods } => {
                let shared = active_shared(&tabs, &active);
                {
                    let term = shared.term.lock();
                    if term.mode().intersects(TermMode::MOUSE_MODE) {
                        // wheel as button 64/65 presses, one per notch
                        let col = (x / geom.cell_w).clamp(0, term.grid().columns() as i32 - 1) as usize;
                        let row = mouse_row(y, &geom).clamp(0, term.grid().screen_lines() as i32 - 1);
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
                    if let Some(w) = &*view.borrow() {
                        w.update();
                    }
                }
            }
            PaintWidgetEvent::Focus(false) => {
                // clicking the titlebar moves focus there; take it back next turn.
                // ponytail: refocus blindly unless a modal/popup is active; revisit when
                // dialogs land (focus proxy is the proper fix then)
                let view = view.clone();
                QTimer::single_shot(0, move || {
                    if DApplication::popup_active() {
                        return;
                    }
                    if let Some(w) = &*view.borrow() {
                        w.set_focus();
                    }
                });
            }
            PaintWidgetEvent::Resize { w, h } => {
                // ponytail: one tiny state.toml write per resize frame, no debounce;
                // add a QTimer debounce if disk churn ever matters
                if !smoke {
                    config::State {
                        window_width: Some(win.width()),
                        window_height: Some(win.height()),
                    }
                    .save();
                }
                if let Some(sb) = *sb_slot.borrow() {
                    sb.as_widget().move_to(w - SB_W, TAB_H);
                    sb.as_widget().resize(SB_W, h - TAB_H);
                }
                let cols = (w / geom.cell_w).max(1) as usize;
                let lines = ((h - TAB_H) / geom.cell_h).max(1) as usize;
                cells.set((cols, lines));
                // every tab follows the window size
                for tab in tabs.borrow().iter() {
                    tab.shared.term.lock().resize(Size { cols, lines });
                    if let Some(m) = &*tab.shared.master.lock().unwrap() {
                        let _ = m.resize(PtySize {
                            rows: lines as u16,
                            cols: cols as u16,
                            pixel_width: w as u16,
                            pixel_height: (h - TAB_H) as u16,
                        });
                    }
                }
            }
            _ => {}
        }
    });
    *view.borrow_mut() = Some(pw.as_widget());

    // DTK tab bar, embedded in the window titlebar (deepin-terminal style)
    let tabbar = DTabBar::new();
    tabbar.set_tabs_closable(true);
    tabbar.set_visible_add_button(true);
    tabbar.set_expanding(false); // deepin-terminal look: compact, content-sized tabs
    tabbar.set_document_mode(true);
    tabbar.as_widget().install_tab_label_style(); // deepin-terminal TermTabStyle equivalent
    *tb_slot.borrow_mut() = Some(tabbar);
    tabbar.connect_signal_i32("currentChanged(int)", {
        let tabs = tabs.clone();
        let active = active.clone();
        let view = view.clone();
        move |i| {
            if i >= 0 && (i as usize) < tabs.borrow().len() {
                active.set(i as usize);
                if let Some(w) = &*view.borrow() {
                    w.update();
                }
            }
        }
    });
    tabbar.connect_signal_i32("tabCloseRequested(int)", {
        let tabs = tabs.clone();
        move |i| {
            if i >= 0 && (i as usize) < tabs.borrow().len() {
                close_tab(&tabs, i as usize);
            }
        }
    });
    tabbar.connect_signal("tabAddRequested()", {
        let cfg = cfg.clone();
        let tabs = tabs.clone();
        let active = active.clone();
        let view = view.clone();
        move || {
            let cwd = active_cwd(&tabs, &active);
            let (cols, lines) = grid_cells(&tabs, &active);
            spawn_tab(&cfg, &tabs, &active, tabbar, &view, cols, lines, cwd);
        }
    });

    // DTK scrollbar, overlaid on the right edge
    let sb = ScrollBar::new(&pw.as_widget());
    // child widgets inherit the parent's I-beam; scrollbar wants the arrow
    sb.as_widget().set_cursor(qt::cursor::ARROW);
    sb.as_widget().show();
    *sb_slot.borrow_mut() = Some(sb);
    sb.on_value_changed({
        let tabs = tabs.clone();
        let active = active.clone();
        let view = view.clone();
        let syncing = syncing_sb.clone();
        move |v| {
            if syncing.get() {
                return; // programmatic sync, not user drag
            }
            let shared = active_shared(&tabs, &active);
            let mut term = shared.term.lock();
            let target = term.grid().history_size() as i32 - v;
            let delta = target - term.grid().display_offset() as i32;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
                drop(term);
                if let Some(w) = &*view.borrow() {
                    w.update();
                }
            }
        }
    });

    win.set_central_widget(&pw.as_widget());
    // remember window size (deepin-terminal window_width/height, konsole state rc)
    let st = config::State::load();
    win.resize(
        st.window_width.unwrap_or(80 * cell_w),
        st.window_height.unwrap_or(24 * cell_h + TAB_H),
    );

    let icon = QIcon::from_theme("deepin-terminal");
    win.set_window_icon(&icon);
    win.as_widget().set_titlebar_icon(&icon);
    win.as_widget().titlebar_set_tabbar(&tabbar.as_widget());
    win.show();

    // spawn the first shell only now: show() delivered the final widget size, so the
    // PTY opens at the real grid size and the shell never gets a startup SIGWINCH
    // (a WINCH after the prompt is drawn makes p10k erase+redraw it = startup flash)
    let (cols, lines) = cells.get();
    // dde-file-manager "open in terminal here" sets our process cwd; portable-pty
    // defaults the shell to $HOME unless we pass it through explicitly
    let cwd = work_dir_arg().or_else(|| {
        std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())
    });
    spawn_tab(&cfg, &tabs, &active, tabbar, &view, cols, lines, cwd);
    // terminal expects immediate keyboard input; DMainWindow focus defaults elsewhere
    pw.set_focus();
    // deepin-terminal look: I-beam over the grid (arrow is the default)
    pw.as_widget().set_cursor(qt::cursor::IBEAM);

    if smoke {
        let view = view.clone();
        let geom = geom.clone();
        let tabs = tabs.clone();
        let tries = Rc::new(std::cell::Cell::new(0));
        let injected = Rc::new(std::cell::Cell::new(false));
        let resized = Rc::new(std::cell::Cell::new(false));
        let spawned2 = Rc::new(std::cell::Cell::new(false));
        let titled = Rc::new(std::cell::Cell::new(false));
        let poll = Rc::new(std::cell::RefCell::new(None::<Box<dyn FnMut()>>));
        let poll2 = poll.clone();
        *poll.borrow_mut() = Some(Box::new(move || {
            let shared = tabs.borrow()[0].shared.clone();
            if !resized.replace(true) {
                // shrink the window: the grid must follow (60 cols)
                // wide enough that prompt+command don't wrap (marker scan is per-line)
                if let Some(w) = &*view.borrow() {
                    w.resize(60 * geom.cell_w, 12 * geom.cell_h);
                }
                let term = shared.term.lock();
                assert_eq!(term.grid().columns(), 60, "grid did not follow window resize");
            }
            if !injected.replace(true) {
                let _ = shared
                    .writer
                    .lock()
                    .unwrap()
                    .write_all(b"echo DTKTERM_SMOKE_OK\n");
            }
            {
                let mut term = shared.term.lock();
                if grid_text(&term).contains("DTKTERM_SMOKE_OK") {
                    // selection + clipboard path: select the marker, copy it
                    let (mut line, mut start, mut end) = (-1i32, 0usize, 0usize);
                    let mut cur = String::new();
                    let mut cur_line = 0i32;
                    for indexed in term.grid().display_iter() {
                        if indexed.point.line.0 != cur_line {
                            cur.clear();
                            cur_line = indexed.point.line.0;
                        }
                        cur.push(indexed.cell.c);
                        if let Some(at) = cur.find("DTKTERM_SMOKE_OK") {
                            // byte offset -> cell column (prompt has multibyte chars)
                            start = cur[..at].chars().count();
                            end = start + "DTKTERM_SMOKE_OK".len() - 1;
                            line = cur_line;
                        }
                    }
                    assert!(line >= 0, "marker wrapped across lines");
                    let mut sel = Selection::new(
                        SelectionType::Simple,
                        Point::new(Line(line), Column(start)),
                        Side::Left,
                    );
                    sel.update(Point::new(Line(line), Column(end)), Side::Right);
                    term.selection = Some(sel);
                    let copied = term.selection_to_string().expect("selection empty");
                    assert!(copied.contains("DTKTERM_SMOKE_OK"), "got: {copied:?}");
                    Clipboard::set_text(&copied);
                    assert!(Clipboard::text().contains("DTKTERM_SMOKE_OK"));
                    // scrollbar synced to the live view: at the prompt == slider at bottom
                    if let Some(sb) = *sb_slot.borrow() {
                        assert_eq!(sb.value(), sb.maximum(), "scrollbar not at bottom");
                    }
                    drop(term);
                    if !spawned2.replace(true) {
                        // exercise tab spawn: a second shell must appear
                        spawn_tab(
                            &cfg,
                            &tabs,
                            &active,
                            tabbar,
                            &view,
                            60,
                            8,
                            None,
                        );
                        assert_eq!(tabs.borrow().len(), 2, "second tab missing");
                        assert_eq!(tabbar.count(), 2, "tabbar count mismatch");
                        // deepin-terminal tab width bounds: long titles elide
                        // instead of stretching the tab (cap 450, floor 110)
                        tabbar.set_tab_text(1, &"x".repeat(500));
                        tabbar.as_widget().flush_layout();
                        let w_long = tabbar.tab_rect(1).width();
                        assert!(w_long <= 450, "tab width {w_long} exceeds 450 cap");
                        let w_short = tabbar.tab_rect(0).width();
                        assert!(w_short >= 110, "tab width {w_short} below 110 floor");
                    }
                    // OSC title -> tab label (sync happens on the next paint)
                    if !titled.replace(true) {
                        // sleep holds off the prompt (and zsh precmd title resets);
                        // second printf paints one letter per SGR attribute for flag asserts
                        let _ = shared
                            .writer
                            .lock()
                            .unwrap()
                            .write_all(concat!(
                                "printf '\\x1b]2;SMOKE_TITLE\\x07'; ",
                                "printf '\\x1b[1mB\\x1b[0m \\x1b[2mD\\x1b[0m \\x1b[3mI\\x1b[0m ",
                                "\\x1b[4mU\\x1b[0m \\x1b[4:3mC\\x1b[0m \\x1b[4:4mP\\x1b[0m ",
                                "\\x1b[4:5mA\\x1b[0m \\x1b[9mS\\x1b[0m \\x1b[4:2mW\\x1b[0m ",
                                "\\x1b[7mR\\x1b[0m \\x1b[8mH\\x1b[0m\\n'; sleep 3\n"
                            ).as_bytes());
                    } else if {
                        let term = shared.term.lock();
                        tabbar.tab_text(0) == "SMOKE_TITLE"
                            && grid_text(&term).contains("B D I U C P A S W R H")
                    } {
                        let term = shared.term.lock();
                        // every alacritty-supported SGR attribute must reach the grid flags
                        use alacritty_terminal::term::cell::Flags;
                        let mut found = None;
                        for indexed in term.grid().display_iter() {
                            if indexed.cell.c == 'B' && indexed.point.line.0 >= 0 {
                                // the attr line starts with B at col 0
                                if indexed.point.column.0 == 0 {
                                    found = Some(indexed.point.line.0);
                                }
                            }
                        }
                        let l = Line(found.expect("attr line"));
                        let flag_at = |col: usize| {
                            term.grid()[l][Column(col)].flags
                        };
                        assert!(flag_at(0).contains(Flags::BOLD), "SGR1");
                        assert!(flag_at(2).contains(Flags::DIM), "SGR2");
                        assert!(flag_at(4).contains(Flags::ITALIC), "SGR3");
                        assert!(flag_at(6).contains(Flags::UNDERLINE), "SGR4");
                        assert!(flag_at(8).contains(Flags::UNDERCURL), "SGR4:3");
                        assert!(flag_at(10).contains(Flags::DOTTED_UNDERLINE), "SGR4:4");
                        assert!(flag_at(12).contains(Flags::DASHED_UNDERLINE), "SGR4:5");
                        assert!(flag_at(14).contains(Flags::STRIKEOUT), "SGR9");
                        assert!(flag_at(16).contains(Flags::DOUBLE_UNDERLINE), "SGR21");
                        assert!(flag_at(18).contains(Flags::INVERSE), "SGR7");
                        assert!(flag_at(20).contains(Flags::HIDDEN), "SGR8");
                        // exercise the real exit path: both shells exit -> app quits
                        println!("smoke ok");
                        for t in tabs.borrow().iter() {
                            let _ = t.shared.writer.lock().unwrap().write_all(b"exit\n");
                        }
                        return;
                    }
                }
            }
            tries.set(tries.get() + 1);
            assert!(tries.get() < 40, "shell output never reached the grid");
            let poll = poll2.clone();
            QTimer::single_shot(250, move || {
                if let Some(f) = &mut *poll.borrow_mut() {
                    f();
                }
            });
        }));
        let poll2 = poll.clone();
        QTimer::single_shot(500, move || {
            if let Some(f) = &mut *poll2.borrow_mut() {
                f();
            }
        });
    }
    pw.leak();
    std::process::exit(app.exec());
}

#[cfg(test)]
mod tests {
    use super::*;
    use dtk::qt::{key, modifier};

    fn kev(k: i32, mods: i32, text: &str) -> KeyEvent {
        KeyEvent { key: k, mods, text: text.into(), press: true, autorepeat: false }
    }

    #[test]
    fn modifiers() {
        assert_eq!(key_bytes(&kev(key::LEFT, 0, ""), false), Some(b"\x1b[D".to_vec()));
        assert_eq!(
            key_bytes(&kev(key::LEFT, modifier::ALT, ""), false),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            key_bytes(&kev(key::RIGHT, modifier::ALT | modifier::SHIFT, ""), false),
            Some(b"\x1b[1;4C".to_vec())
        );
        assert_eq!(key_bytes(&kev(key::BACKSPACE, 0, ""), false), Some(b"\x7f".to_vec()));
        assert_eq!(
            key_bytes(&kev(key::BACKSPACE, modifier::ALT, ""), false),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(key_bytes(&kev(i32::from(b'L'), modifier::CONTROL, "\x0c"), false), Some(vec![0x0c]));
        assert_eq!(
            key_bytes(&kev(i32::from(b'F'), modifier::ALT, "f"), false),
            Some(b"\x1bf".to_vec())
        );
    }

    #[test]
    fn app_cursor_keys() {
        // DECCKM on: plain arrows go SS3; modified arrows keep CSI
        assert_eq!(key_bytes(&kev(key::UP, 0, ""), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(&kev(key::DOWN, 0, ""), true), Some(b"\x1bOB".to_vec()));
        assert_eq!(key_bytes(&kev(key::UP, 0, ""), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            key_bytes(&kev(key::UP, modifier::SHIFT, ""), true),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn shift_tab_is_csi_z() {
        // xterm: Shift+Tab (Backtab) is always plain CSI Z, never mod-encoded
        assert_eq!(key_bytes(&kev(key::BACKTAB, modifier::SHIFT, ""), false), Some(b"\x1b[Z".to_vec()));
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
        assert_eq!(find_url_span("see http://x and https://y.z", 20), Some((17, 28)));
    }
}
