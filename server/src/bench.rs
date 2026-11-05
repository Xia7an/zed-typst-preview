//! `typst-livepreview bench <file>`: measures the keystroke → pixels path
//! (incremental compile + re-rasterizing the pages that changed), without a
//! window.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rayon::prelude::*;
use typst_layout::PagedDocument;

use crate::diff;
use crate::render::{Bitmap, page_px, render_page, render_rows, runs};
use crate::world::{self, PreviewWorld};

/// Pixels per point of a fit-to-width A4 page in a half-screen Retina window.
const SCALE: f64 = 2.6;
/// Strip height used by Zed's preview pane.
const STRIP_PX: u32 = 128;

fn compile(world: &mut PreviewWorld) -> anyhow::Result<(PagedDocument, Duration)> {
    world.begin_revision();
    let t = Instant::now();
    let doc = typst::compile::<PagedDocument>(&*world)
        .output
        .map_err(|e| anyhow::anyhow!("compile error: {}", e.first().map(|d| d.message.as_str()).unwrap_or("?")))?;
    let elapsed = t.elapsed();
    typst::comemo::evict(10);
    Ok((doc, elapsed))
}

fn hashes(doc: &PagedDocument) -> Vec<u128> {
    doc.pages().iter().map(|p| typst::utils::hash128(&(&p.frame, &p.fill))).collect()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn stats(name: &str, v: &mut [f64]) {
    v.sort_by(f64::total_cmp);
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    println!("  {name:<22} median {:>7.2} ms   p90 {:>7.2} ms   max {:>7.2} ms", p(0.5), p(0.9), p(1.0));
}

pub fn run(path: &Path) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path).with_context(|| format!("{}", path.display()))?;
    let text = std::fs::read_to_string(&path)?;

    let t = Instant::now();
    let fonts = Arc::new(world::load_fonts(&[], true));
    println!("fonts loaded:            {:>8.2} ms", ms(t.elapsed()));

    let mut world = PreviewWorld::new(fonts, Arc::new(world::packages()));
    world.set_overlay(&path, text.clone());
    anyhow::ensure!(world.set_target(path.parent().unwrap(), &path), "bad path");

    let (doc, cold) = compile(&mut world)?;
    println!("cold compile:            {:>8.2} ms  ({} pages)", ms(cold), doc.pages().len());
    let t = Instant::now();
    doc.pages().par_iter().for_each(|p| drop(render_page(p, SCALE, false)));
    println!("render all pages:        {:>8.2} ms  (parallel, {SCALE} px/pt)", ms(t.elapsed()));
    let mut prev = hashes(&doc);

    // Type into the middle of the first prose line, then delete again.
    let offset = find_prose_line_end(&text).context("no prose line to edit")?;
    let (mut c, mut r, mut total) = (Vec::new(), Vec::new(), Vec::new());
    let (mut d, mut rs, mut total_s) = (Vec::new(), Vec::new(), Vec::new());
    let (mut bytes_full, mut bytes_strips) = (0usize, 0usize);
    let mut changed_pages = 0;
    let mut prev_doc = doc;
    let mut problems = 0;
    for i in 0..60 {
        let start = Instant::now();
        // Type a new character each time (no cache hits from earlier states).
        let src = world.overlay_mut(&path).unwrap();
        let ch = TYPED.as_bytes()[i % TYPED.len()] as char;
        let at = offset + i;
        src.edit(at..at, ch.encode_utf8(&mut [0; 4]));
        let (doc, compile_time) = compile(&mut world)?;
        let now = hashes(&doc);
        let changed: Vec<usize> = (0..now.len()).filter(|&i| prev.get(i) != Some(&now[i])).collect();
        changed_pages += changed.len();
        let after_compile = start.elapsed();

        // Before: re-rasterize every changed page.
        let t = Instant::now();
        let full: Vec<Bitmap> = changed.par_iter().map(|&i| render_page(&doc.pages()[i], SCALE, false)).collect();
        let render_time = t.elapsed();

        // Now: diff the layout, re-rasterize only the strips that changed.
        let t = Instant::now();
        let jobs: Vec<(usize, Vec<u32>)> = changed
            .iter()
            .map(|&i| {
                let page = &doc.pages()[i];
                let (_, h) = page_px(page, SCALE);
                let strips = match prev_doc.pages().get(i).and_then(|old| diff::dirty_bands(old, page)) {
                    Some(bands) => diff::dirty_strips(&bands, SCALE, h, STRIP_PX),
                    None => (0..h.div_ceil(STRIP_PX)).collect(),
                };
                (i, strips)
            })
            .collect();
        let diff_time = t.elapsed();
        let t = Instant::now();
        let strips: Vec<(usize, u32, Bitmap)> = jobs
            .par_iter()
            .flat_map(|(i, strips)| {
                let page = &doc.pages()[*i];
                let h = page_px(page, SCALE).1;
                runs(strips, STRIP_PX)
                    .into_par_iter()
                    .map(move |run| (*i, run.first, render_rows(page, SCALE, false, run.rows(h))))
            })
            .collect();
        let strip_time = t.elapsed();

        bytes_full += full.iter().map(Bitmap::bytes).sum::<usize>();
        bytes_strips += strips.iter().map(|(_, _, b)| b.bytes()).sum::<usize>();
        problems += check(&prev_doc, &changed, &full, &strips);

        prev = now;
        prev_doc = doc;
        if i >= 4 {
            c.push(ms(compile_time));
            r.push(ms(render_time));
            total.push(ms(after_compile + render_time));
            d.push(ms(diff_time));
            rs.push(ms(strip_time));
            total_s.push(ms(after_compile + diff_time + strip_time));
        }
    }
    let edits = 60.0;
    println!("keystroke → pixels ({} edits, {:.1} changed pages/edit):", c.len(), changed_pages as f64 / edits);
    stats("incremental compile", &mut c);
    println!(" whole changed pages ({:.0} KiB of pixels/edit):", bytes_full as f64 / edits / 1024.0);
    stats("render changed pages", &mut r);
    stats("total", &mut total);
    println!(" changed strips only ({:.0} KiB of pixels/edit, {STRIP_PX} px strips):", bytes_strips as f64 / edits / 1024.0);
    stats("layout diff", &mut d);
    stats("render changed strips", &mut rs);
    stats("total", &mut total_s);
    anyhow::ensure!(problems == 0, "{problems} pixel mismatches (see log)");
    println!("check: strips match full renders, unchanged strips match the old pixels");
    Ok(())
}

/// Verifies incremental rendering against full renders: the strips must
/// equal the same rows of the full page, and all other rows must equal the
/// old page. Returns the number of problems found.
fn check(old: &PagedDocument, changed: &[usize], full: &[Bitmap], strips: &[(usize, u32, Bitmap)]) -> usize {
    let mut problems = 0;
    for (&i, page) in changed.iter().zip(full) {
        fn row(b: &Bitmap, y: u32) -> &[u32] {
            let w = b.width as usize;
            &b.pixels[y as usize * w..(y as usize + 1) * w]
        }
        // Rows that come from strips.
        let mut fresh = vec![false; page.height as usize];
        for (_, first, b) in strips.iter().filter(|(p, _, _)| *p == i) {
            let y0 = first * STRIP_PX;
            for y in 0..b.height {
                fresh[(y0 + y) as usize] = true;
                let (count, max) = channel_diff(row(b, y), row(page, y0 + y));
                // A different translation changes float rounding, which can
                // flip one anti-aliasing sample of an edge pixel here and there.
                if count > 4 || max > 64 {
                    eprintln!("page {} row {}: {count} pixels differ from the full render (max {max})", i + 1, y0 + y);
                    problems += 1;
                } else if count > 0 {
                    log::debug!("page {} row {}: {count} pixels differ by up to {max}", i + 1, y0 + y);
                }
            }
        }
        let Some(old_page) = old.pages().get(i) else { continue };
        let before = render_page(old_page, SCALE, false);
        if (before.width, before.height) != (page.width, page.height) {
            continue;
        }
        for y in 0..page.height {
            if !fresh[y as usize] && row(&before, y) != row(page, y) {
                eprintln!("page {} row {y}: changed but not re-rendered", i + 1);
                problems += 1;
            }
        }
    }
    problems
}

/// Number of pixels that differ by more than 2 levels, and the largest
/// difference of a channel.
fn channel_diff(a: &[u32], b: &[u32]) -> (usize, u32) {
    let ch = |p: u32, s: u32| (p >> s) & 0xff;
    let diffs = a.iter().zip(b).map(|(&p, &q)| [0, 8, 16].iter().map(|&s| ch(p, s).abs_diff(ch(q, s))).max().unwrap_or(0));
    diffs.fold((0, 0), |(n, m), d| (n + (d > 2) as usize, m.max(d)))
}

const TYPED: &str = " the quick brown fox jumps over the lazy dog and keeps typing";

/// Byte offset at the end of the first line that looks like plain text.
fn find_prose_line_end(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        let prose = t.chars().next().is_some_and(|c| c.is_alphabetic())
            && !t.contains('[')
            && !t.contains(']')
            && !t.contains('$');
        if prose {
            return Some(offset + line.trim_end().len());
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"#set page(width: 300pt, height: 420pt, margin: 20pt, fill: gradient.linear(white, luma(235)))
#set text(size: 10pt)
= Heading

First paragraph with some words that wrap across a couple of lines in this narrow page. EDIT1

$ sum_(k=1)^n k = (n (n + 1)) / 2 + "EDIT2" $

#table(columns: 3, [a], [b], [EDIT3], [d], [e], [f])

#rotate(12deg)[Rotated EDIT4]

#block(clip: true, height: 14pt, width: 80pt, fill: gradient.linear(red, blue))[Clipped EDIT5 #lorem(10)]

#lorem(40) EDIT6

#place(bottom + right)[EDIT7]
"#;

    #[test]
    fn incremental_render_matches_full_render() {
        let dir = std::env::temp_dir().join(format!("tlp-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let path = dir.join("main.typ");
        std::fs::write(&path, DOC).unwrap();
        let mut world = PreviewWorld::new(Arc::new(world::load_fonts(&[], false)), Arc::new(world::packages()));
        world.set_overlay(&path, DOC.to_string());
        assert!(world.set_target(&dir, &path));

        let (mut prev, _) = compile(&mut world).unwrap();
        let edits = [
            ("EDIT1", "x"),
            ("EDIT1", " and a much longer insertion that makes the paragraph reflow onto more lines"),
            ("EDIT2", " + 1"),
            ("EDIT3", "wider cell"),
            ("EDIT4", "!"),
            ("EDIT5", "?"),
            ("EDIT6", "tail"),
            ("EDIT7", "corner"),
            ("Heading", " changed"),
        ];
        let mut diffed = 0;
        for (marker, text) in edits {
            let src = world.overlay_mut(&path).unwrap();
            let at = src.text().find(marker).unwrap();
            // Insert after the marker (keeps it for later edits).
            let at = at + marker.len();
            src.edit(at..at, text);
            let (doc, _) = compile(&mut world).unwrap();
            let changed: Vec<usize> = (0..doc.pages().len())
                .filter(|&i| prev.pages().get(i).map(|p| hashes_of(p)) != Some(hashes_of(&doc.pages()[i])))
                .collect();
            let full: Vec<Bitmap> = changed.iter().map(|&i| render_page(&doc.pages()[i], SCALE, false)).collect();
            let mut strips = Vec::new();
            for &i in &changed {
                let page = &doc.pages()[i];
                let h = page_px(page, SCALE).1;
                let dirty = match prev.pages().get(i).and_then(|old| diff::dirty_bands(old, page)) {
                    Some(bands) => {
                        diffed += 1;
                        diff::dirty_strips(&bands, SCALE, h, STRIP_PX)
                    }
                    None => (0..h.div_ceil(STRIP_PX)).collect(),
                };
                for run in runs(&dirty, STRIP_PX) {
                    strips.push((i, run.first, render_rows(page, SCALE, false, run.rows(h))));
                }
            }
            assert_eq!(check(&prev, &changed, &full, &strips), 0, "after replacing {marker:?}");
            prev = doc;
        }
        assert!(diffed > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hashes_of(p: &typst_layout::Page) -> u128 {
        typst::utils::hash128(&(&p.frame, &p.fill))
    }
}
