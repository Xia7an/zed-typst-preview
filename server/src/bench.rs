//! `typst-livepreview bench <file>`: measures the keystroke → pixels path
//! (incremental compile + re-rasterizing the pages that changed), without a
//! window.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rayon::prelude::*;
use typst_layout::PagedDocument;

use crate::render::render_page;
use crate::world::{self, PreviewWorld};

/// Pixels per point of a fit-to-width A4 page in a half-screen Retina window.
const SCALE: f64 = 2.6;

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
    let mut changed_pages = 0;
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
        let t = Instant::now();
        changed.par_iter().for_each(|&i| drop(render_page(&doc.pages()[i], SCALE, false)));
        let render_time = t.elapsed();
        prev = now;
        if i >= 4 {
            c.push(ms(compile_time));
            r.push(ms(render_time));
            total.push(ms(start.elapsed()));
        }
    }
    println!("keystroke → pixels ({} edits, {:.1} changed pages/edit):", c.len(), changed_pages as f64 / 60.0);
    stats("incremental compile", &mut c);
    stats("render changed pages", &mut r);
    stats("total", &mut total);
    Ok(())
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
