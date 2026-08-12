# deptty — agent guide

deepin-terminal rewritten in Rust on DTK6. Goal: drop-in replacement for
deepin-terminal. Upstream bindings live in [dtk-rs](https://github.com/st0nie/dtk-rs)
(pulled as a git dependency, not a path dep).

## Layout

```
src/main.rs    thin entry point: main() -> deptty::main_run()
src/lib.rs     the whole app: window, tabs, split-pane tree, render loop, input,
               PTY reader threads, right-click menu
src/config.rs  Config + KeyBinding + Action, TOML loading, defaults
locales/       rust-i18n YAML (en, zh-CN); menu strings via t!("menu.*")
tests/app.rs   headless full-session test (offscreen): shells, splits, tab move
Cargo.toml     dtk (git), alacritty_terminal, portable-pty, serde/toml, dirs
examples/      scratch probes (font metrics etc.), not part of the app
ARCHITECTURE.md  design: component mapping, threading/tab model, render/input pipeline
```

Deliberately one big lib.rs for now; split into modules only when a second
widget (search bar) forces it.

## Commands

```sh
cargo build                                        # build
./target/debug/deptty                              # run
cargo test                                         # unit + headless integration (offscreen)
```

The integration test (`tests/app.rs`) boots the real app offscreen, spawns real
shells, asserts grid content, splits, tab reorder, and the exit path. Run it
after touching render/input/pty/pane code. Never hold a `tabs` borrow across
widget calls that fire events synchronously (set_focus/show/resize) — the event
handlers borrow `tabs` too and a borrow_mut will panic (reentrancy).

## Hard rules

- Requires Linux with Qt6 + DTK6 dev packages (dtk-rs links `dtk6widget`). Not cross-platform.
- Rust edition 2024; keep `unsafe`-adjacent lints clean.
- **Single GUI thread.** All dtk wrappers are `!Send`; never touch widgets or
  `Tab`/`Rc` state from reader threads. Cross-thread traffic is exactly one
  byte per event on the per-tab `UnixStream` socketpair; GUI side handles it
  in `QSocketNotifier`.
- Terminal state access only via `shared.term.lock()` (`FairMutex`). Reader
  thread holds it across `Processor::advance`; GUI locks briefly per paint —
  keep GUI-side lock scopes tiny.
- Tab close = `SIGHUP` to shell pid; UI removal happens on the reader thread's
  EOF poke (`'q'`). Don't remove tabs directly — one code path for "shell gone".
- Scrollbar sync has a re-entry guard (`syncing` Cell) — keep it.
- Don't re-add a path dependency on dtk-rs; hack on bindings upstream and push.
- `target/`, `.codegraph/` are gitignored scratch — don't commit.

## Style

- Minimal diffs. New deepin-terminal features (splits, search, themes, quake)
  follow the existing pattern: state in `Shared`/`Tab`, bytes in, paint out.
- Config keys: add to `src/config.rs` with a serde default; bad/missing config
  must never crash (load() falls back to defaults).
- Keybindings go through `config::Action` + `KeyBinding`, not hardcoded key checks.
- Follow the roadmap order in ARCHITECTURE.md unless told otherwise.
