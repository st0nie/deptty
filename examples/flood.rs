//! CPU flood probe: stream output at a pane, sample process CPU.
//! usage: cargo run --example flood [lines_per_sec]
//! Writes N lines then keeps a spinner-ish stream going; prints %CPU of
//! self every 500ms via /proc/self/stat.
use deptty::config::Config;
use deptty::dtk::*;
use deptty::{boot, App};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn main() {
    let flood = std::env::args().nth(1).unwrap_or("continuous".into());
    unsafe {
        if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
        }
    }
    let cfg = Config { shell: Some("/bin/bash".into()), repaint_delay: 10, ..Default::default() };
    let (dapp, app) = boot(cfg);

    // read our own CPU time
    let cpu = Rc::new(Cell::new(0u64));
    let last = Rc::new(Cell::new(0u64));
    let t0 = Rc::new(Cell::new(std::time::Instant::now()));
    let mut last_print = std::time::Instant::now();
    let done = Rc::new(Cell::new(false));

    let poll = Rc::new(RefCell::new(None::<Box<dyn FnMut()>>));
    let poll2 = poll.clone();
    let cpu2 = cpu.clone();
    let last2 = last.clone();
    let t02 = t0.clone();
    let done2 = done.clone();

    let boot_shared = app.tabs.borrow()[0].panes[0].shared.clone();
    let bs = boot_shared.clone();
    *poll.borrow_mut() = Some(Box::new({
        let flood = flood.clone();
        move || {
        let shared = bs.clone();
        match flood.as_str() {
            "burst" => {
                // one giant dump: 10MB of lines as fast as the pty drains
                let mut out = String::with_capacity(10 << 20);
                for i in 0..200_000 {
                    out.push_str(&format!("line {i} of the flood 0123456789abcdef\n"));
                }
                shared.write(out.as_bytes());
            }
            "spinner" => {
                // real TUI pattern: a background printf loop rewrites one line
                // with \r; the shell echoes it, so the terminal decodes + repaints
            }
            _ => {
                // continuous: nothing per tick; a background shell loop floods
            }
        }
        // CPU sample: /proc/self/stat utime+stime in ticks
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let after_cm = stat.rsplit(')').next().unwrap().trim_start();
        let f: Vec<&str> = after_cm.split_whitespace().collect();
        let ticks: u64 = f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap(); // utime, stime
        cpu2.set(ticks);
        let now = std::time::Instant::now();
        if now.duration_since(last_print) >= std::time::Duration::from_millis(500) {
            last_print = now;
            let el = now.duration_since(t02.get()).as_secs_f64();
            let dt = (ticks - last2.get()) as f64;
            let sysconf = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
            let pct = dt / sysconf / 0.5 * 100.0;
            println!("t={el:5.1}s  self-cpu={pct:5.1}%");
            last2.set(ticks);
        }
        if now.duration_since(t02.get()) > std::time::Duration::from_secs(6) {
            println!("probe done, quitting");
            done2.set(true);
            DApplication::quit(); // direct: shell-exit path is flaky in probes
            return;
        }
        let poll = poll2.clone();
        QTimer::single_shot(5, move || {
            if let Some(f) = &mut *poll.borrow_mut() {
                f();
            }
        });
        }
    }));
    let poll2 = poll.clone();
    QTimer::single_shot(200, move || {
        if let Some(f) = &mut *poll2.borrow_mut() {
            f();
        }
    });
    // spinner mode: run the \r rewrite loop inside the shell, in the
    // background, so the terminal streams real TUI spinner output
    if flood == "spinner" {
        boot_shared.write(
            b"while true; do for c in '|' '/' '-' '\\'; do printf '\\rspinning %s 0123456789abcdef' \"$c\"; sleep 0.005; done; done &\n",
        );
    } else {
        // continuous: a background loop streams new lines; probe only samples
        boot_shared.write(
            b"while true; do echo '0123456789abcdef stream line'; sleep 0.005; done &\n",
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    dapp.exec();
    assert!(done.get(), "probe never finished");
}
