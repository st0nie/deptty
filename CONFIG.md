# deptty configuration

deptty is configured with a TOML file at:

- `$XDG_CONFIG_HOME/deptty/config.toml`, or
- `~/.config/deptty/config.toml` when `XDG_CONFIG_HOME` is unset.

**Every key is optional.** A missing file, a missing key, or even a file with
unknown keys never breaks startup: missing/unknown keys fall back to their
defaults, and a syntactically bad file is skipped with a warning on stderr
(deptty then runs on full defaults). This makes it safe to keep your config
across versions that add new options.

```
~/.config/deptty/
├── config.toml      # everything below
├── state.toml       # remembered window size (written by deptty, don't edit)
└── themes/          # your own colorschemes, see "Themes"
    └── my-theme.toml
```

---

## Quick reference

```toml
# ---- display ----
font_family = "Fira Code"   # optional, default: system monospace
font_size = 13              # point size, default 12
cursor_shape = "beam"       # "block" | "beam" | "underline", default "block"

# ---- terminal ----
shell = "/bin/zsh"          # optional, default: $SHELL, then /bin/bash
scrollback = 20000          # history lines kept in memory, default 10000

# ---- colors ----
theme = "breeze"            # optional, default: system dark/light palette
                            # see "Themes" for names, paths, and the format

# ---- key bindings (optional, see "Key bindings") ----
[[key_bindings]]
key = "T"
mods = "Ctrl+Shift"
action = "new_tab"
```

---

## Options

### `font_family` — string, optional

Font used for the terminal grid. It **must be a monospace font** or the cell
grid will not align. Default: the system's default monospace font.

```toml
font_family = "JetBrains Mono"
```

### `font_size` — integer, default `12`

Font size in points. Combined with `font_family`, this determines the grid
cell size and therefore how many columns/rows fit in the window.

### `cursor_shape` — string, default `"block"`

Shape of the text cursor **in the focused pane**. One of:

| value       | looks like            |
|-------------|-----------------------|
| `"block"`   | `█` solid block       |
| `"beam"`    | `|` vertical bar      |
| `"underline"` | `_` underline bar   |

Unfocused panes always draw a hollow block, as a focus hint (deepin-terminal
behavior). There is no blink option.

```toml
cursor_shape = "beam"
```

### `shell` — string, optional

Shell launched in every new pane. Default: `$SHELL`, falling back to
`/bin/bash`.

```toml
shell = "/usr/bin/fish"
```

### `scrollback` — integer, default `10000`

How many lines of scrollback history the emulator keeps in memory. This is
the history you can reach with the scrollbar or the mouse wheel when the
application is not scrolling itself.
when the application is not scrolling itself.

```toml
scrollback = 50000
```

### `theme` — string, optional

Colorscheme. A theme name like `"breeze"`, or the path to a `.toml` theme
file. When unset (default), deptty uses the classic deepin palette and picks
dark or light automatically from the system window color. See [Themes](#themes).

```toml
theme = "breeze"                 # a name (user dir → system dir → built-in)
theme = "~/.config/deptty/themes/gruvbox.toml"   # a direct path
```

---

## Key bindings

Key bindings are defined by `[[key_bindings]]` tables. Each entry has three
fields:

| field    | meaning                                                       |
|----------|---------------------------------------------------------------|
| `key`    | single character (`"T"`, `";"`) or a Qt key name (see below)  |
| `mods`   | `"+"`-joined modifiers: `Ctrl`, `Shift`, `Alt`                |
| `action` | what the binding does (see below)                             |

```toml
[[key_bindings]]
key = "T"
mods = "Ctrl+Shift"
action = "new_tab"
```

### Modifiers

Any combination of `Ctrl`, `Shift`, `Alt`, joined with `+`:

```toml
mods = "Ctrl+Shift"   # Ctrl + Shift
mods = "Alt"          # Alt only
mods = "Ctrl+Alt"     # Ctrl + Alt
```

An empty `mods` (`mods = ""`) or omitting it binds the bare key — but bare
printable keys are consumed by the shell, so you almost always want at least
one modifier.

### Key names

- **Single characters**: `"T"`, `";"`, `"1"`, `"F"` — matched
  case-insensitively (`"t"` == `"T"`).
- **Named keys** (Qt names, case-insensitive):

  `Escape` / `Esc`, `Tab`, `Backtab` (Shift+Tab), `Backspace`,
  `Return` / `Enter`, `Left`, `Right`, `Up`, `Down`, `Home`, `End`,
  `Delete`, `Insert`, `PageUp`, `PageDown`.

  Function keys (`F1`…`F12`) are not yet boundable.

### Actions

| action               | effect                                              |
|----------------------|-----------------------------------------------------|
| `copy`               | copy the current selection to the clipboard         |
| `paste`              | paste clipboard content into the focused shell      |
| `new_tab`            | open a new tab (inherits the focused shell's cwd)   |
| `close_tab`          | close the focused pane's tab (default Ctrl+Shift+W)   |
| `close_workspace`    | close the focused workspace (split); not bound by default, use for one-key split close |
| `next_tab`           | switch to the next tab                              |
| `prev_tab`           | switch to the previous tab                          |
| `split_horizontal`   | split the focused pane with a horizontal divider (top/bottom) |
| `split_vertical`     | split the focused pane with a vertical divider (left/right)   |
| `next_pane`          | cycle focus to the next split pane (leaf order)     |
| `prev_pane`          | cycle focus to the previous split pane              |
| `focus_pane_up`      | move focus to the pane above                        |
| `focus_pane_down`    | move focus to the pane below                        |
| `focus_pane_left`    | move focus to the pane to the left                  |
| `focus_pane_right`   | move focus to the pane to the right                 |

### Defaults

When no `[[key_bindings]]` are configured, deptty ships these (modeled on
deepin-terminal / konsole):

| key                    | mods           | action              |
|------------------------|----------------|---------------------|
| `C`                    | `Ctrl+Shift`   | `copy`              |
| `V`                    | `Ctrl+Shift`   | `paste`             |
| `T`                    | `Ctrl+Shift`   | `new_tab`           |
| `W`                    | `Ctrl+Shift`   | `close_tab`         |
| `Right`                | `Shift`        | `next_tab`          |
| `Left`                 | `Shift`        | `prev_tab`          |
| `(`                    | `Ctrl+Shift`   | `split_vertical`    |
| `)`                    | `Ctrl+Shift`   | `split_horizontal`  |
| `Tab`                  | `Ctrl`         | `next_pane`         |
| `Backtab`              | `Ctrl+Shift`   | `prev_pane`         |
| `Up`                   | `Ctrl+Shift`   | `focus_pane_up`     |
| `Down`                 | `Ctrl+Shift`   | `focus_pane_down`   |
| `Left`                 | `Ctrl+Shift`   | `focus_pane_left`   |
| `Right`                | `Ctrl+Shift`   | `focus_pane_right`  |

**Rebinding**: entries are matched in order; the first binding whose key +
modifiers match wins. To rebind, just add your own `[[key_bindings]]` — your
entries are additive, and any match stops the lookup, so a custom binding
with the same key as a default overrides it. There is currently no way to
*remove* a default binding.

Example — switch tabs with `Alt+1`/`Alt+2` style, keep everything else:

```toml
[[key_bindings]]
key = "1"
mods = "Alt"
action = "prev_tab"   # note: any binding on "1" also stops the shell from
                      # receiving Alt+1, which shells usually ignore anyway

[[key_bindings]]
key = "2"
mods = "Alt"
action = "next_tab"
```

---

## Themes

Colorschemes are ghostty-style TOML files. A theme is selected with the
`theme` key (see above) and looked up, in order:

1. `~/.config/deptty/themes/<name>.toml` — **your themes, take precedence**
2. `/usr/share/deptty/themes/<name>.toml` — system themes (the `.deb`
   package installs `breeze.toml` here)
3. the embedded `breeze` theme — always available, even before install

A missing or unparseable theme file is not an error: deptty prints a warning
and falls back to the default palette.

### Shipped themes

The repo (and the `.deb`) ship these, all ported from their official
upstream sources (each file's header names the exact source):

| name | source |
|---|---|
| `breeze` | KDE Konsole `Breeze.colorscheme` |
| `solarized-dark` / `solarized-light` | Ethan Schoonover's Solarized + author's xresources port |
| `dracula` | Dracula spec, official Alacritty port |
| `nord` | Nord palette, official Xresources port |
| `gruvbox-dark` | morhetz/gruvbox `gruvbox.vim` |
| `one-dark` | atom/one-dark-syntax `colors.less` |
| `tokyo-night` | folke/tokyonight.nvim Alacritty port |

Pick one with `theme = "nord"`, or copy any file to your themes dir and
edit it — your copy wins over the shipped one.

### Theme file format

Theme files use the same field conventions as [ghostty
themes](https://ghostty.org/docs/features/theme): kebab-case keys and
`#rrggbb` (or `rrggbb`) hex colors.

```toml
# ~/.config/deptty/themes/my-theme.toml
background = "#1e1e2e"     # terminal background
foreground = "#cdd6f4"     # default text color
cursor-color = "#f5e0dc"   # text-cursor overlay color

# The 16 ANSI palette colors: index 0-7 normal, 8-15 bright.
# Array form (one entry per color):
palette = [
  "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
  "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]
```

The palette can also be written in ghostty's newer `N = "#..."` table form,
either as an inline table or as dotted keys — both are valid TOML and both
are accepted:

```toml
# inline table
palette = { 0 = "#45475a", 1 = "#f38ba8", 8 = "#585b70", 15 = "#a6adc8" }

# or dotted keys
palette.0 = "#45475a"
palette.1 = "#f38ba8"
```

Every field is optional; anything you leave out falls back to the default
palette's value for that slot. A palette with fewer than 16 entries is
ignored entirely (all 16 are required). Unknown keys are **ignored**, so
stock ghostty theme files load unmodified — ghostty's `cursor-text`,
`selection-background`, `selection-foreground` and any other keys parse fine.
Note that deptty paints selection as a translucent overlay and the cursor as
a translucent tint, so `cursor-color` is used as the tint base and the
selection keys are accepted but have no visual effect (yet).

### Built-in theme: Breeze

`breeze` is the shipped default theme, ported from KDE Konsole's Breeze
terminal scheme:

```toml
background = "#232627"
foreground = "#fcfcfc"
cursor-color = "#eff0f1"
```

It is always available under the name `breeze` (embedded in the binary),
installed to `/usr/share/deptty/themes/breeze.toml` by the package, and can
be overridden by dropping your own `~/.config/deptty/themes/breeze.toml`.

### Writing your own theme

Copy any existing theme and edit the hex values:

```sh
mkdir -p ~/.config/deptty/themes
cp themes/breeze.toml ~/.config/deptty/themes/my-theme.toml
```

then point `theme = "my-theme"` at it. Useful palettes can be borrowed from
ghostty's theme collection (alacritty/kitty palettes are easy to convert —
each has 16 ANSI colors, foreground and background).

### How the theme is applied

The resolved theme sets, in order:

1. `background` → the viewport background; also picks dark vs light mode
   (luminance of the background), which controls overlay tints
2. `foreground` → default text color
3. `cursor-color` → the cursor overlay tint
4. `palette` → the 16 ANSI colors used by `NamedColor` lookups, bold
   brightening, and 256-color index 0-15

---

## Remembered state (`state.toml`)

deptty remembers the last window size between runs in
`~/.config/deptty/state.toml`:

```toml
window_width = 1280
window_height = 800
```

This file is **written by deptty** and re-applied at startup. It lives
outside `config.toml` so that your hand-written comments and key ordering in
the config survive. Deleting it just forgets the window size.

---

## Behavior notes

- **Bad config, never crash.** Malformed TOML in `config.toml` or a missing
  theme: warning on stderr, defaults used. Unknown keys and unknown theme
  fields are ignored silently.
- **Keybindings match first-wins** in the order they appear in the file.
- **A binding consumes the key**: matched bindings never reach the shell;
  unmatched keys are forwarded to the shell as escape sequences (with
  `$TERM=xterm-256color`).
- **Split panes**: same-axis splits share space equally (two splits →
  thirds), closing a pane rebalances the axis and focuses the split sibling.
- **The tab label** follows the focused pane's title (`OSC 0`/`OSC 2`), and
  re-applies immediately when pane focus changes; panes that never set a
  title show the shell name.
