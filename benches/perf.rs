//! perf probes, run via `cargo bench --bench perf` (no criterion: custom
//! harness that prints min/med/avg/max itself). Two measurements:
//!
//! 1. `parse`: alacritty `Processor::advance` over real `ls --color=always
//!    -al` output (~540KB colored / 470KB plain) — pure reader-thread cost.
//! 2. `drain`: end-to-end `time ls --color=always -al` through the real
//!    offscreen app; parses bash's `real 0m0.057s` line from the grid (ls
//!    wall time incl. PTY blocking) over N runs.

use deptty::alacritty_terminal::event::VoidListener;
use deptty::alacritty_terminal::term::{self, Term};
use deptty::alacritty_terminal::vte::ansi::{Handler, Processor, StdSyncHandler};
use deptty::config::Config;
use deptty::dtk::*;
use deptty::{App, grid_text};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() {
    parse_bench();
    drain_timing();
}

// ---- 1. parser throughput ------------------------------------------------

struct StdNoop;
impl Handler for StdNoop {}

struct Dim {
    cols: usize,
    lines: usize,
}
impl deptty::alacritty_terminal::grid::Dimensions for Dim {
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

fn parse_bench() {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        println!("parse: skip (no HOME)");
        return;
    }
    let colored = std::process::Command::new("ls")
        .args(["--color=always", "-al"])
        .current_dir(&home)
        .output()
        .unwrap()
        .stdout;
    let plain = std::process::Command::new("ls")
        .args(["-al"])
        .current_dir(&home)
        .output()
        .unwrap()
        .stdout;
    println!(
        "parse: colored {}B plain {}B (from {home})",
        colored.len(),
        plain.len()
    );

    let run = |name: &str, data: &[u8], scrollback: usize| {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let cfg = term::Config {
                scrolling_history: scrollback,
                ..Default::default()
            };
            let mut term = Term::new(cfg, &Dim { cols: 100, lines: 30 }, VoidListener);
            let mut p = Processor::<StdSyncHandler>::new();
            let t0 = Instant::now();
            for chunk in data.chunks(65536) {
                p.advance(&mut term, std::hint::black_box(chunk));
            }
            best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
        }
        println!("parse: advance({name}, hist={scrollback}) best {best:.2}ms");
    };
    run("colored", &colored, 10_000);
    run("plain", &plain, 10_000);
    run("colored", &colored, 0); // history growth vs base parse

    // pure vte parse, no Term write: handler is a no-op
    let mut best = f64::MAX;
    for _ in 0..5 {
        let mut p = Processor::<StdSyncHandler>::new();
        let t0 = Instant::now();
        for chunk in colored.chunks(65536) {
            p.advance(&mut StdNoop, std::hint::black_box(chunk));
        }
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
    }
    println!("parse: pure vte (no Term) best {best:.2}ms");
}

// ---- 2. end-to-end drain timing ------------------------------------------

fn drain_timing() {
    unsafe {
        std::env::set_var("QT_QPA_PLATFORM", "offscreen");
        let tmp = std::env::temp_dir().join(format!("deptty-perf-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
    }
    let tmp = std::env::temp_dir().join(format!("deptty-perf-{}", std::process::id()));

    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        println!("drain: skip (no HOME)");
        return;
    }

    let cfg = Config {
        shell: Some("/bin/bash".into()),
        ..Default::default()
    };
    let (dapp, app) = deptty::boot(cfg);
    let n_runs = 30;
    let results = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(Cell::new(false));
    let poll = Rc::new(RefCell::new(None::<Box<dyn FnMut()>>));
    let poll2 = poll.clone();
    *poll.borrow_mut() = Some(Box::new({
        let app: App = app.clone();
        let results = results.clone();
        let done = done.clone();
        let mut iter = 0usize;
        let mut armed = false; // true between runs: waiting to start next
        let mut cmd_at = None::<Instant>;
        let mut out = String::new();
        move || {
            let tab0 = app.tabs.borrow()[0].panes[0].shared.clone();
            if !armed {
                tab0.write(format!("cd {home}\n").as_bytes());
                armed = true;
            } else if cmd_at.is_none() {
                cmd_at = Some(Instant::now());
                // marker computed by the shell so the echoed command line
                // ("echo DONE$((10+0))") cannot false-match DONE10
                tab0.write(
                    format!("time ls --color=always -al; echo DONE$((10+{iter}))\n").as_bytes(),
                );
            } else {
                // scan for this run's fresh completion: DONE{iter} after a
                // fresh `real` row (previous runs' rows have scrolled off)
                let (real_row, has_done) = {
                    let term = tab0.term().lock();
                    let mut real_row = None;
                    let mut has_done = false;
                    let offset = term.grid().display_offset() as i32;
                    let mut cur = String::new();
                    let mut cur_line = i32::MIN;
                    for indexed in term.grid().display_iter() {
                        if indexed.point.line.0 + offset < 0 {
                            continue;
                        }
                        if indexed.point.line.0 != cur_line {
                            cur_line = indexed.point.line.0;
                            cur.clear();
                        }
                        cur.push(indexed.cell.c);
                        if cur.starts_with("real\t") || cur.starts_with("real ") {
                            real_row = Some(cur.clone());
                        }
                        if cur.contains(&format!("DONE{}", 10 + iter)) {
                            has_done = true;
                        }
                    }
                    (real_row, has_done)
                };
                if has_done && real_row.is_some() {
                    let secs = real_row
                        .and_then(|r| {
                            let b = r.as_bytes();
                            let p = b.windows(2).position(|w| w[0] == b'm' && w[1].is_ascii_digit())?;
                            let mut num = String::new();
                            for c in &b[p + 1..] {
                                let c = *c as char;
                                if c.is_ascii_digit() || c == '.' {
                                    num.push(c);
                                } else {
                                    break;
                                }
                            }
                            num.parse::<f64>().ok()
                        })
                        .unwrap_or(f64::NAN);
                    let el = cmd_at.take().unwrap().elapsed().as_secs_f64() * 1000.0;
                    results.borrow_mut().push((secs, el));
                    out.push_str(&format!("run {:2} real={:.3}s elapsed={:.1}ms\n", iter, secs, el));
                    iter += 1;
                    if iter >= n_runs {
                        let mut v: Vec<f64> = results.borrow().iter().map(|r| r.0).collect();
                        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        let med = v[n_runs / 2];
                        let avg: f64 = v.iter().sum::<f64>() / n_runs as f64;
                        out.push_str(&format!(
                            "drain: real min={:.3} med={:.3} avg={:.3} max={:.3} (n={})\n",
                            v[0], med, avg, v[n_runs - 1], n_runs
                        ));
                        println!("{out}");
                        done.set(true);
                        for t in app.tabs.borrow().iter() {
                            for p in t.panes.iter() {
                                p.shared.write(b"exit\n");
                            }
                        }
                        return;
                    }
                    cmd_at = None; // arm next run
                } else if cmd_at.is_some_and(|t| t.elapsed() > Duration::from_secs(20)) {
                    let tail: String = grid_text(&tab0.term().lock())
                        .chars()
                        .rev()
                        .take(400)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    println!("drain: TIMEOUT waiting for DONE{iter}; grid tail: {tail:?}");
                    done.set(true);
                    return;
                }
            }
            let poll = poll2.clone();
            QTimer::single_shot(25, move || {
                if let Some(f) = &mut *poll.borrow_mut() {
                    f();
                }
            });
        }
    }));
    let poll2 = poll.clone();
    QTimer::single_shot(100, move || {
        if let Some(f) = &mut *poll2.borrow_mut() {
            f();
        }
    });
    dapp.exec();
    assert!(done.get(), "drain: session never completed");
    let _ = std::fs::remove_dir_all(&tmp);
}
