# deptty — agent guide

deepin-terminal rewritten in Rust on DTK6. Goal: drop-in replacement for
deepin-terminal. Upstream bindings live in [dtk-rs](https://github.com/st0nie/dtk-rs)
(pulled as a git dependency, not a path dep).

## Layout

```
src/main.rs    the whole app (~1500 lines): window, tabs, render loop, input,
               PTY reader threads, smoke mode
src/config.rs  Config + KeyBinding + Action, TOML loading, defaults
Cargo.toml     dtk (git), alacritty_terminal, portable-pty, serde/toml, dirs
examples/      scratch probes (font metrics etc.), not part of the app
ARCHITECTURE.md  design: component mapping, threading/tab model, render/input pipeline
```

Deliberately one big main.rs for now; split into modules only when a second
widget (splits/search bar) forces it.

## Commands

```sh
cargo build                                        # build
./target/debug/deptty                              # run
QT_QPA_PLATFORM=offscreen ./target/debug/deptty --smoke   # headless smoke test
```

No test suite; `--smoke` (spawns a real shell, asserts `DTKTERM_SMOKE_OK`
appears in the grid) is the check. Run it after touching render/input/pty code.

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
