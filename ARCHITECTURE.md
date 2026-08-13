# deptty architecture

deepin-terminal rewritten in Rust on DTK6. Goal: a drop-in replacement —
same UX (tabs in titlebar, quake mode, remote management), no C++.

## Component mapping

| deepin-terminal (C++) | deptty (Rust) | Notes |
|---|---|---|
| qtermwidget (deepin fork) | `alacritty_terminal` | battle-tested VT parser + screen grid + scrollback; replaces the whole fork |
| KPty / fork+exec | `portable-pty` | maintained PTY wrapper |
| `TerminalDisplay.cpp` (QPainter cell grid) | `render()` on `dtk::PaintWidget` | same approach, drawn cell-by-cell with `Painter::draw_text_at` |
| DSettings / QSettings / DConfig | serde + TOML, `~/.config/deptty/config.toml` | `src/config.rs` |
| DTabBar in DTitlebar | `dtk::widgets::DTabBar` | one tab = one shell |
| QtDBus (single instance, KWin, appearance) | `zbus` (planned) | |
| chardet + QTextCodec | `chardetng` + `encoding_rs` (planned) | |
| libsecret (SSH passwords) | `oo7` (planned) | |

Not used on purpose: qtermwidget (C++ fork; deepin's patches get redone in
Rust), VTE (GTK-only), GPU rendering (QPainter is what the original uses;
plenty fast).

## Layers

```
shell <-> portable-pty <-> reader thread --bytes--> Processor::advance(FairMutex<Term>)
                                |
                        UnixStream poke  (one socketpair per tab)
                                v
              QSocketNotifier (GUI thread) -> PaintWidget.update()
                                v
        paintEvent -> render(): grid cells -> Painter runs (bg rect + text)
key/IME -> KeyEvent -> escape bytes (or config keybinding action) -> pty writer
```

## Threading & tab model

- Single GUI thread owns all widgets and all painting (Qt rule, dtk-rs
  wrappers are `!Send`).
- Per tab: one `Tab { shared: Arc<Shared>, pid }`. `Shared` holds the
  `FairMutex<Term<TitleListener>>` plus the pty writer.
- Reader thread per tab: blocks on `pty.read()`, feeds bytes into
  `Processor::advance(term)`, then pokes the GUI thread by writing one byte
  to a `UnixStream` socketpair. On EOF it pokes `'q'` instead.
- GUI thread: one `QSocketNotifier` per tab — byte = `update()` (repaint),
  `'q'` = shell exited, remove tab. No polling, no cross-thread widget calls.
- Closing a tab = `SIGHUP` to the shell pid; the reader thread's EOF poke
  does the UI removal. One code path for "shell gone".

Tab state lives in `Rc<RefCell<Vec<Tab>>>` + `Rc<Cell<usize>>` active index —
GUI-thread only, no `Send` needed.

## Rendering

`render()` iterates the visible grid, groups consecutive cells with the same
style into runs, and draws one background rect + one `draw_text_at` per run.
Color resolution: `cell_colors` -> `rgb_of` against the resolved `Scheme`
(named 16-color palette + indexed 256 cube + RGB) — the default deepin
dark/light palette, or the palette of a `theme = "..."` colorscheme, see
[Themes](#themes). Bold/underline/italic map to `QFont` variants; cursor is
an inverted rect.

Geometry: `QFont::metrics()` gives cell width/height/ascent (`GridGeom`);
widget pixel size / cell size = grid cols/lines, fed to alacritty's
`Dimensions` impl and to `pty.resize()` on resize events.

## Input

- Keys: `key_bytes()` maps `KeyEvent` -> VT escape sequences (app-cursor mode
  respected). Config keybindings are checked first (`KeyBinding::key_code` +
  `mod_mask`); matched bindings run an `Action`, unmatched keys go to the pty.
- IME input forwards as UTF-8 bytes.
- Mouse: if the app enabled mouse reporting (vim/htop), events become SGR
  reports; otherwise drag = selection (`alacritty_terminal::selection`),
  typing clears it. Wheel: scrolls history, or button 64/65 reports when the
  app owns the mouse.
- Scrollbar mirrors `Term` scroll state (`sync_scrollbar`), with a re-entry
  guard (`syncing` flag).

## dtk-rs features built for this

- `PaintWidget`: fully user-drawn widget; paint/key/mouse/wheel/IME/resize/
  focus all forward to one Rust handler (`dtk/examples/paint_widget.rs` is
  the self-check).
- `QFont::metrics()` / `advance()`: cell geometry.
- `Painter::draw_text_at`: baseline text for grid rendering.
- `Clipboard`: copy/paste.
- `DTabBar`: tabs in the titlebar.

## Config

TOML at `$XDG_CONFIG_HOME/deptty/config.toml` (falls back to
`~/.config/deptty/config.toml`). Bad file = warning + defaults, never a
crash. See README for the key list; `src/config.rs` is the source of truth.

## Themes

Colorschemes are ghostty-style TOML files resolved once at boot
(`scheme_for` in `src/lib.rs`, loading in `src/config.rs`):

1. `~/.config/deptty/themes/<name>.toml` (user, wins)
2. `/usr/share/deptty/themes/<name>.toml` (system; the .deb installs
   `themes/breeze.toml` there)
3. embedded `breeze` default (dev builds, before install)

`theme` in config.toml takes a name or a direct file path; missing/unknown
themes and bad TOML warn and fall back to the default palette — never crash.
Theme files use ghostty field conventions (`background`, `foreground`,
`cursor-color`, `palette` = 16 ANSI colors, array or `N = "#..."` table
form); extra ghostty keys are ignored. Selection is always the translucent
overlay, so `selection-*` / `cursor-text` keys parse but are unused.

## Roadmap (deepin-terminal parity)

Widget classes are all bound in dtk-rs; what's missing is application code:

1. ~~splits~~ done (binary Node tree per tab, ratio-draggable dividers; no DSplitter)
2. search bar
3. ~~themes~~ done (ghostty-style TOML colorschemes, see Themes); opacity still planned
4. quake mode (zbus + KWin)
5. single instance (zbus)
6. remote management / SSH (oo7 secret storage, encoding detection)
