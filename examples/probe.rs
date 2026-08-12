// probe: real shaped advance vs grid cell width
fn main() {
    let app = dtk::DApplication::new("probe");
    let _ = &app;
    for family in ["Fira Code", "monospace"] {
        let f = dtk::QFont::new();
        f.set_family(family);
        f.set_point_size(12);
        let (cw, ch, _) = f.metrics();
        let digits = "2".repeat(20);
        let adv = f.advance(&digits);
        let adv1 = f.advance("2");
        println!(
            "{family}: cell_w={cw} cell_h={ch} adv('2')={adv1} adv(20 x '2')={adv} -> per-char {:.3}",
            adv as f64 / 20.0
        );
        let fb = {
            let b = dtk::QFont::new();
            b.set_family(family);
            b.set_point_size(12);
            b.set_bold(true);
            b
        };
        println!(
            "{family} bold: adv('2')={} adv(20)={} per-char {:.3}",
            fb.advance("2"),
            fb.advance(&digits),
            fb.advance(&digits) as f64 / 20.0
        );
    }
}
