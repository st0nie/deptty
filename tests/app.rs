//! headless full-session test (replaces --smoke): real DTK window offscreen,
//! real shells, driven through the Qt event loop by a poll timer.
//!
//! Covers: boot + first shell, resize -> grid follow, marker echo, selection
//! copy to clipboard, scrollbar sync, second tab, tab width caps, OSC title ->
//! tab label, SGR attribute flags, vertical + horizontal splits (new shells in
//! new panes), tab drag reorder (tabMoved), and the pane-exit teardown path.
//!
//! Run: QT_QPA_PLATFORM=offscreen cargo test --test app
use deptty::alacritty_terminal::grid::Dimensions as _;
use deptty::alacritty_terminal::index::{Column, Line, Point, Side};
use deptty::alacritty_terminal::selection::{Selection, SelectionType};
use deptty::alacritty_terminal::term::cell::Flags;
use deptty::config::Config;
use deptty::dtk::*;
use deptty::{App, grid_text};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

#[test]
fn headless_full_session() {
    // SAFETY: single test in this process, set before any thread/Qt init.
    // deepin sessions export QT_QPA_PLATFORM=dxcb;xcb — override unconditionally,
    // the test must always be headless
    unsafe {
        std::env::set_var("QT_QPA_PLATFORM", "offscreen");
        // never touch the user's real config/state
        let tmp = std::env::temp_dir().join(format!("deptty-test-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
    }
    let tmp = std::env::temp_dir().join(format!("deptty-test-{}", std::process::id()));

    let cfg = Config {
        shell: Some("/bin/bash".into()), // deterministic: no precmd/title tricks
        ..Default::default()
    };
    let (dapp, app) = deptty::boot(cfg);
    let geom = app.geom.clone();

    let done = Rc::new(Cell::new(false));
    let tries = Rc::new(Cell::new(0));
    let injected = Rc::new(Cell::new(false));
    let resized = Rc::new(Cell::new(false));
    let spawned2 = Rc::new(Cell::new(false));
    let titled = Rc::new(Cell::new(false));
    let split1 = Rc::new(RefCell::new(None::<Arc<deptty::Shared>>));
    let split2 = Rc::new(RefCell::new(None::<Arc<deptty::Shared>>));
    let poll = Rc::new(RefCell::new(None::<Box<dyn FnMut()>>));
    let poll2 = poll.clone();
    *poll.borrow_mut() = Some(Box::new({
        let app: App = app.clone();
        let done = done.clone();
        move || {
            let tab0 = app.tabs.borrow()[0].panes[0].shared.clone();
            if !resized.replace(true) {
                // shrink the window: the grid must follow (60 cols)
                // wide enough that prompt+command don't wrap (marker scan is per-line)
                app.root.as_widget().resize(60 * geom.cell_w, 12 * geom.cell_h);
                let term = tab0.term().lock();
                assert_eq!(term.grid().columns(), 60, "grid did not follow window resize");
            }
            if !injected.replace(true) {
                tab0.write(b"echo DTKTERM_SMOKE_OK\n");
            }
            let tab1_active_pane = || -> Option<u64> {
                let ts = app.tabs.borrow();
                ts.get(1).map(|t| t.active_pane.get())
            };
            {
                let mut term = tab0.term().lock();
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
                    let sb = app.tabs.borrow()[0].panes[0].sb;
                    assert_eq!(sb.value(), sb.maximum(), "scrollbar not at bottom");
                    drop(term);
                    if !spawned2.replace(true) {
                        // exercise tab spawn: a second shell must appear
                        deptty::spawn_tab(&app, None);
                        assert_eq!(app.tabs.borrow().len(), 2, "second tab missing");
                        assert_eq!(app.tabbar.count(), 2, "tabbar count mismatch");
                        // deepin-terminal tab width bounds: long titles elide
                        // instead of stretching the tab (cap 450, floor 110)
                        app.tabbar.set_tab_text(1, &"x".repeat(500));
                        app.tabbar.as_widget().flush_layout();
                        let w_long = app.tabbar.tab_rect(1).width();
                        assert!(w_long <= 450, "tab width {w_long} exceeds 450 cap");
                        let w_short = app.tabbar.tab_rect(0).width();
                        assert!(w_short >= 110, "tab width {w_short} below 110 floor");
                        // no stale scroll-button gap left of the first tab
                        assert_eq!(app.tabbar.tab_rect(0).x(), 0, "stale scroll gap before first tab");
                        // splits: vertical (left/right) on tab 1's only pane,
                        // then horizontal (top/bottom) on the new pane
                        let p0 = tab1_active_pane().expect("tab 1 pane");
                        let s1 = deptty::split_pane(&app, p0, true).expect("vertical split");
                        assert_eq!(app.tabs.borrow()[1].panes.len(), 2, "vertical split missing");
                        let p1 = tab1_active_pane().expect("new pane");
                        let s2 = deptty::split_pane(&app, p1, false).expect("horizontal split");
                        assert_eq!(app.tabs.borrow()[1].panes.len(), 3, "horizontal split missing");
                        s1.write(b"echo DTKTERM_VSPLIT_OK\n");
                        s2.write(b"echo DTKTERM_HSPLIT_OK\n");
                        *split1.borrow_mut() = Some(s1);
                        *split2.borrow_mut() = Some(s2);
                    }
                    // OSC title -> tab label (sync happens on the next paint)
                    if !titled.replace(true) {
                        // sleep holds off the prompt (and zsh precmd title resets);
                        // second printf paints one letter per SGR attribute for flag asserts
                        tab0.write(concat!(
                            "printf '\\x1b]2;SMOKE_TITLE\\x07'; ",
                            "printf '\\x1b[1mB\\x1b[0m \\x1b[2mD\\x1b[0m \\x1b[3mI\\x1b[0m ",
                            "\\x1b[4mU\\x1b[0m \\x1b[4:3mC\\x1b[0m \\x1b[4:4mP\\x1b[0m ",
                            "\\x1b[4:5mA\\x1b[0m \\x1b[9mS\\x1b[0m \\x1b[4:2mW\\x1b[0m ",
                            "\\x1b[7mR\\x1b[0m \\x1b[8mH\\x1b[0m\\n'; sleep 3\n"
                        ).as_bytes());
                    } else {
                        let split_ok = |s: &Rc<RefCell<Option<Arc<deptty::Shared>>>>| {
                            s.borrow().as_ref().is_some_and(|s| {
                                grid_text(&s.term().lock()).contains("SPLIT_OK")
                            })
                        };
                        let term = tab0.term().lock();
                        if app.tabbar.tab_text(0) == "SMOKE_TITLE"
                            && grid_text(&term).contains("B D I U C P A S W R H")
                            && split_ok(&split1)
                            && split_ok(&split2)
                        {
                            // every alacritty-supported SGR attribute must reach the grid flags
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
                            let flag_at = |col: usize| term.grid()[l][Column(col)].flags;
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
                            drop(term);
                            // tab drag reorder: QTabBar::moveTab emits tabMoved,
                            // the tabs vec must mirror the new bar order
                            app.tabbar.move_tab(1, 0);
                            {
                                let ts = app.tabs.borrow();
                                assert_eq!(ts[0].panes.len(), 3, "split tab not at index 0");
                                assert_eq!(ts[1].panes.len(), 1, "plain tab not at index 1");
                            }
                            // the moved tab stays current; active index tracks it
                            assert_eq!(app.tabbar.current_index(), 0);
                            assert_eq!(app.active.get(), 0);
                            // context menu: builds, localizes, pops; its actions
                            // are the same fns the keybinding paths exercise above
                            let pane_id = app.tabs.borrow()[1].panes[0].id;
                            let pane_w = app.tabs.borrow()[1].panes[0].pw.as_widget();
                            deptty::context_menu(&app, pane_id, &pane_w, 10, 10);
                            assert!(DApplication::popup_active(), "menu did not pop");
                            // exercise the real exit path: every shell exits -> app quits
                            done.set(true);
                            for t in app.tabs.borrow().iter() {
                                for p in t.panes.iter() {
                                    p.shared.write(b"exit\n");
                                }
                            }
                            return;
                        }
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
        }
    }));
    let poll2 = poll.clone();
    QTimer::single_shot(500, move || {
        if let Some(f) = &mut *poll2.borrow_mut() {
            f();
        }
    });
    dapp.exec();
    assert!(done.get(), "session never completed (event loop exited early?)");
    let _ = std::fs::remove_dir_all(&tmp);
}
