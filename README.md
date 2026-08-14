# deptty

deepin-terminal rewritten in Rust on DTK6 (via [dtk-rs](https://github.com/st0nie/dtk-rs)).

Goal: a drop-in replacement for deepin-terminal — same UX, no C++.

## Status

Working today:

- terminal emulation (VT parsing, screen grid, scrollback) via `alacritty_terminal`
- PTY via `portable-pty`
- tabs (in the titlebar, like deepin-terminal), new/close/switch, tab drag reorder,
  cwd inheritance
- split panes: vertical/horizontal dividers, equalized sizing, pane focus cycling
  and directional focus, right-click menu (new tab/split/close)
- text selection + copy/paste, configurable keybindings
- scrollback scrollbar + mouse wheel, mouse reporting for vim/htop
- colorschemes: ghostty-style TOML themes, `breeze` built in (see below)
- window title from OSC 0/2, tab labels (follow the focused pane)
- config file at `~/.config/deptty/config.toml`
- remembered window size (`state.toml`, like deepin-terminal's window_width/height)
- headless integration test (`cargo test`, real shells offscreen)

Not yet: search bar, opacity, quake mode, remote management (SSH),
single-instance via zbus, encoding detection, secret storage.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and the mapping of each
deepin-terminal component to its Rust replacement.

## Build & run

Requires Linux with Qt6 + DTK6 dev packages (dtk-rs links `dtk6widget`).

```sh
cargo build
./target/debug/deptty                  # run
./target/debug/deptty -w /tmp          # start shell in a dir
./target/debug/deptty /tmp             # same, positional dir
./target/debug/deptty 'file:///tmp'    # dde-file-manager "open in terminal here"
cargo test                             # unit + headless full-session test
QT_QPA_PLATFORM=offscreen cargo test --test app   # just the headless session
```

The integration test boots the real app offscreen (`QT_QPA_PLATFORM=offscreen`),
spawns real shells, and asserts grid content, splits, tab reorder, selection,
scrollbar sync, OSC titles, and the exit path.

## .deb package

[cargo-deb](https://github.com/kornelski/cargo-deb) (shared-lib deps via
`dpkg-shlibdeps` — no dh-cargo):

```sh
cargo install cargo-deb
cargo deb        # -> target/debian/deptty_<ver>_amd64.deb
```

## Config

`~/.config/deptty/config.toml` (all keys optional) — see
[**CONFIG.md**](CONFIG.md) for the full reference: every option, keybinding
syntax and defaults, cursor shapes, and the theme format.

```toml
font_family = "Fira Code"
font_size = 13
scrollback = 20000
shell = "/bin/zsh"
cursor_shape = "block"  # block | beam | underline (focused pane; unfocused
                        # panes always show a hollow block, deepin-terminal style)
theme = "breeze"        # colorscheme; omit for the default deepin palette

[[key_bindings]]
key = "T"            # single char or qt key name: "Left", "PageUp", "Escape", ...
mods = "Ctrl+Shift"  # "+"-joined: Ctrl, Shift, Alt
action = "new_tab"   # copy | paste | new_tab | close_tab | close_workspace | next_tab | prev_tab
                     # split_horizontal | split_vertical | next_pane | prev_pane
                     # focus_pane_up | focus_pane_down | focus_pane_left | focus_pane_right
```

Defaults: Ctrl+Shift+C/V copy/paste, Ctrl+Shift+T new tab, Ctrl+Shift+W close
tab, Shift+Left/Right switch tab (konsole), Ctrl+Shift+( / Ctrl+Shift+)
split left-right / top-bottom, Ctrl+Tab / Ctrl+Shift+Tab cycle split panes,
Ctrl+Shift+Up/Down/Left/Right move focus between split panes (konsole
Focus * Terminal). Same-axis splits share space equally (two splits =
thirds); closing a pane focuses its split sibling and rebalances the rest.

## Themes

`theme = "name"` picks a colorscheme; missing/unknown names fall back to
the default palette (no crash). Themes are ghostty-style TOML files looked
up in order:

1. `~/.config/deptty/themes/<name>.toml` (yours, wins)
2. `/usr/share/deptty/themes/<name>.toml` (system, installed by the .deb)
3. `breeze` is embedded as a last resort, so it always works

Shipped themes (all ported from their official upstream sources, credited in
file headers): `breeze` (KDE Konsole), `solarized-dark`, `solarized-light`
(Ethan Schoonover), `dracula` (draculatheme.com), `nord` (Nord / Arctic Ice
Studio), `gruvbox-dark` (morhetz), `one-dark` (atom/one-dark-syntax),
`tokyo-night` (folke/tokyonight.nvim).

`theme` can also be a direct path to a `.toml`. The repo ships
[`themes/breeze.toml`](themes/breeze.toml); copy it to your themes dir and
edit, or drop in a ghostty theme file (extra keys are ignored):

```toml
background = "#232627"
foreground = "#fcfcfc"
cursor-color = "#eff0f1"
# 16 ANSI colors; array form, or ghostty's N = "#..." table form
palette = [
  "#232627", "#ed1515", "#11d116", "#f67400",
  "#1d99f3", "#9b59b6", "#1abc9c", "#fcfcfc",
  "#7f8c8d", "#c0392b", "#1cdc9a", "#fdbc4b",
  "#3daee9", "#8e44ad", "#16a085", "#ffffff",
]
```

## Layout

```
src/main.rs    thin entry: main() -> deptty::main_run()
src/lib.rs     the whole app: window, tabs, split-pane tree, render loop,
               input, PTY reader threads, right-click menu
src/config.rs  Config + KeyBinding + Theme, TOML loading, defaults
locales/       rust-i18n YAML (en, zh-CN); menu strings via t!("menu.*")
tests/app.rs   headless full-session test (offscreen)
themes/        shipped colorschemes, installed to /usr/share/deptty/themes
examples/      scratch probes (font metrics etc.), not part of the app
```

The terminal widget itself is one `dtk::PaintWidget`; see ARCHITECTURE.md for
the data flow.

## Emoji

Emoji render via fontconfig fallback to Noto Color Emoji. If they show as tofu
boxes, install `fonts-noto-color-emoji` and add a strong fallback, e.g.
`~/.config/fontconfig/conf.d/10-emoji-fallback.conf`:

```xml
<match target="pattern">
  <test name="family"><string>monospace</string></test>
  <edit name="family" mode="append" binding="strong"><string>Noto Color Emoji</string></edit>
</match>
```
