//! SyncTeX (`<job>.synctex.gz`): maps source lines to positions on the pages
//! and back.
//!
//! The file lists the boxes TeX shipped out, each tagged with the input file
//! and line it came from. Positions are in scaled points; converted here to
//! PDF points (bp) from the top-left of the page:
//! `(raw + offset) × Unit × Magnification / 1000 / 65781.76`. pdfTeX & co.
//! write offsets of 0; DVI engines (upLaTeX) put TeX's 1in origin there.
//!
//! SyncTeX only knows lines (the column is always -1), so positions within a
//! line are estimated from the order of the records of that line.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Scaled points per big point.
const SP_PER_BP: f64 = 65781.76;

/// A place on a page (0-based), in points from the top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marker {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug)]
struct HBox {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

impl HBox {
    fn contains(&self, x: f32, y: f32, tolerance: f32) -> bool {
        x >= self.left - tolerance && x <= self.right + tolerance && y >= self.top - tolerance && y <= self.bottom + tolerance
    }

    fn area(&self) -> f32 {
        (self.right - self.left).max(0.0) * (self.bottom - self.top).max(0.0)
    }
}

/// A point record (character run, glue, kern, math, void box) inside an hbox.
#[derive(Clone, Copy, Debug)]
struct Leaf {
    tag: u32,
    line: u32,
    /// 1-based, as in the file.
    page: u32,
    x: f32,
    y: f32,
    hbox: u32,
}

#[derive(Default)]
pub struct SyncTex {
    /// Input tag → path, for the inputs `parse` was told are sources.
    inputs: HashMap<u32, PathBuf>,
    hboxes: Vec<HBox>,
    /// In document order, only leaves of source inputs.
    leaves: Vec<Leaf>,
}

/// Raw values as read, before the unit conversion (which may only be known
/// at the end, in the post scriptum).
struct Raw {
    unit: f64,
    mag: f64,
    x_offset: f64,
    y_offset: f64,
}

fn int(s: &str) -> Option<i64> {
    s.trim().parse().ok()
}

/// `tag,line[,column]:rest` → (tag, line, rest).
fn link(s: &str) -> Option<(u32, u32, &str)> {
    let (link, rest) = s.split_once(':')?;
    let mut it = link.split(',');
    let tag = it.next()?.trim().parse().ok()?;
    let line = it.next()?.trim().parse().ok()?;
    Some((tag, line, rest))
}

/// `h,v[:W,H,D]` → (h, v, [W, H, D]).
fn point_and_size(s: &str) -> Option<(i64, i64, Vec<i64>)> {
    let (pos, size) = match s.split_once(':') {
        Some((p, s)) => (p, Some(s)),
        None => (s, None),
    };
    let (h, v) = pos.split_once(',')?;
    let size = size.map(|s| s.split(',').filter_map(int).collect()).unwrap_or_default();
    Some((int(h)?, int(v)?, size))
}

impl SyncTex {
    /// Reads a (decompressed) SyncTeX file. `source` maps an input path as
    /// written in the file to the source file it stands for, or `None` for
    /// inputs that are not sources (classes, packages, aux files).
    pub fn parse(reader: impl BufRead, mut source: impl FnMut(&str) -> Option<PathBuf>) -> Self {
        struct RawBox {
            h: i64,
            v: i64,
            w: i64,
            height: i64,
            depth: i64,
        }
        struct RawLeaf {
            tag: u32,
            line: u32,
            page: u32,
            h: i64,
            v: i64,
            hbox: u32,
        }
        let mut raw = Raw { unit: 1.0, mag: 1000.0, x_offset: 0.0, y_offset: 0.0 };
        let mut inputs = HashMap::new();
        let mut boxes: Vec<RawBox> = Vec::new();
        let mut leaves: Vec<RawLeaf> = Vec::new();
        // Enclosing boxes: `Some(index)` for hboxes, `None` for vboxes.
        let mut stack: Vec<Option<u32>> = Vec::new();
        let mut page = 0u32;

        for line in reader.split(b'\n') {
            let Ok(line) = line else { break };
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            let Some(first) = line.chars().next() else { continue };
            let body = &line[first.len_utf8()..];
            match first {
                '{' => {
                    page = int(body).unwrap_or(page as i64 + 1) as u32;
                    stack.clear();
                }
                '}' => stack.clear(),
                '[' => stack.push(None),
                ']' | ')' => {
                    stack.pop();
                }
                '(' => {
                    let Some((_, _, rest)) = link(body) else { continue };
                    let Some((h, v, size)) = point_and_size(rest) else { continue };
                    let [w, height, depth] = size[..] else { continue };
                    boxes.push(RawBox { h, v, w, height, depth });
                    stack.push(Some((boxes.len() - 1) as u32));
                }
                'x' | 'g' | 'k' | '$' | 'h' | 'v' => {
                    let Some(Some(hbox)) = stack.last().copied() else { continue };
                    let Some((tag, line, rest)) = link(body) else { continue };
                    let Some((h, v, _)) = point_and_size(rest) else { continue };
                    leaves.push(RawLeaf { tag, line, page, h, v, hbox });
                }
                'I' => {
                    if let Some(rest) = line.strip_prefix("Input:")
                        && let Some((tag, path)) = rest.split_once(':')
                        && let Ok(tag) = tag.parse::<u32>()
                        && let Some(path) = source(path)
                    {
                        inputs.insert(tag, path);
                    }
                }
                _ => {
                    // Header and post scriptum (the latter overrides).
                    let Some((key, value)) = line.split_once(':') else { continue };
                    let value = value.trim().parse::<f64>();
                    match (key, value) {
                        ("Unit", Ok(v)) if v > 0.0 => raw.unit = v,
                        ("Magnification", Ok(v)) if v > 0.0 => raw.mag = v,
                        ("X Offset", Ok(v)) => raw.x_offset = v,
                        ("Y Offset", Ok(v)) => raw.y_offset = v,
                        _ => {}
                    }
                }
            }
        }

        let k = raw.unit * raw.mag / 1000.0 / SP_PER_BP;
        let x = |h: i64| ((h as f64 + raw.x_offset) * k) as f32;
        let y = |v: i64| ((v as f64 + raw.y_offset) * k) as f32;
        let len = |l: i64| (l as f64 * k) as f32;
        let hboxes = boxes
            .iter()
            .map(|b| HBox {
                left: x(b.h),
                right: x(b.h) + len(b.w),
                top: y(b.v) - len(b.height),
                bottom: y(b.v) + len(b.depth),
            })
            .collect();
        let leaves = leaves
            .iter()
            .filter(|l| inputs.contains_key(&l.tag))
            .map(|l| Leaf { tag: l.tag, line: l.line, page: l.page, x: x(l.h), y: y(l.v), hbox: l.hbox })
            .collect();
        Self { inputs, hboxes, leaves }
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// The position of `line` (1-based) of `path`, `frac` (0..1) of the way
    /// through that line.
    pub fn forward(&self, path: &Path, line: u32, frac: f64) -> Option<Marker> {
        let tags: HashSet<u32> = self.inputs.iter().filter(|(_, p)| *p == path).map(|(t, _)| *t).collect();
        if tags.is_empty() {
            return None;
        }
        let ours: Vec<&Leaf> = self.leaves.iter().filter(|l| tags.contains(&l.tag)).collect();
        // The line itself, else the closest line before it, else after it.
        let best = ours
            .iter()
            .map(|l| l.line)
            .min_by_key(|&l| if l <= line { (0, line - l) } else { (1, l - line) })?;
        let on_line: Vec<&Leaf> = ours.into_iter().filter(|l| l.line == best).collect();
        // Running heads repeat lines on later pages: use the first page.
        let page = on_line.first()?.page;
        let on_page: Vec<&Leaf> = on_line.into_iter().filter(|l| l.page == page).collect();
        let frac = if best == line { frac.clamp(0.0, 1.0) } else { 0.0 };
        let i = ((frac * on_page.len() as f64) as usize).min(on_page.len() - 1);
        Some(self.marker(on_page[i]))
    }

    fn marker(&self, leaf: &Leaf) -> Marker {
        let hbox = self.hboxes[leaf.hbox as usize];
        let (top, bottom) = if hbox.bottom - hbox.top >= 1.0 {
            (hbox.top, hbox.bottom)
        } else {
            (leaf.y - 8.0, leaf.y + 2.0)
        };
        Marker {
            page: leaf.page.saturating_sub(1) as usize,
            x: leaf.x as f64,
            y: top as f64,
            height: (bottom - top) as f64,
        }
    }

    /// The source location at `x`, `y` (points) on `page` (0-based): the
    /// path, the line (1-based) and how far through that line (0..1).
    pub fn backward(&self, page: usize, x: f64, y: f64) -> Option<(PathBuf, u32, f64)> {
        let page = page as u32 + 1;
        let (x, y) = (x as f32, y as f32);
        let on_page: Vec<&Leaf> = self.leaves.iter().filter(|l| l.page == page).collect();
        // The innermost line box under the point, if any.
        let inner = on_page
            .iter()
            .map(|l| l.hbox)
            .filter(|&b| self.hboxes[b as usize].contains(x, y, 0.5))
            .min_by(|&a, &b| self.hboxes[a as usize].area().total_cmp(&self.hboxes[b as usize].area()));
        let leaf = match inner {
            Some(hbox) => {
                let in_box = || on_page.iter().filter(move |l| l.hbox == hbox);
                in_box()
                    .filter(|l| l.x <= x + 0.5)
                    .max_by(|a, b| a.x.total_cmp(&b.x))
                    .or_else(|| in_box().min_by(|a, b| a.x.total_cmp(&b.x)))
                    .copied()
            }
            None => on_page
                .iter()
                .min_by(|a, b| self.distance(a, x, y).total_cmp(&self.distance(b, x, y)))
                .copied(),
        }?;
        // Where the leaf is among the records of its line.
        let same: Vec<&Leaf> = on_page.iter().filter(|l| l.tag == leaf.tag && l.line == leaf.line).copied().collect();
        let index = same.iter().position(|l| std::ptr::eq(*l, leaf)).unwrap_or(0);
        let frac = index as f64 / same.len().max(1) as f64;
        Some((self.inputs.get(&leaf.tag)?.clone(), leaf.line, frac))
    }

    /// How far `leaf` is from a click, mostly vertically.
    fn distance(&self, leaf: &Leaf, x: f32, y: f32) -> f32 {
        let b = self.hboxes[leaf.hbox as usize];
        let dy = if y < b.top { b.top - y } else { (y - b.bottom).max(0.0) };
        let dx = if x >= leaf.x { x - leaf.x } else { 2.0 * (leaf.x - x) };
        4.0 * dy + dx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From a pdfLaTeX run (`synctex view` agrees on the converted numbers).
    const PDF: &str = "SyncTeX Version:1
Input:1:/p/main.tex
Input:2:/texmf/article.cls
Output:pdf
Magnification:1000
Unit:1
X Offset:0
Y Offset:0
Content:
!932
{1
[1,6:4736286,46220574:26673152,41484288,0
[1,6:8799518,44254494:22609920,36044800,0
(1,4:8799518,8865054:22609920,447828,0
h1,3:8799518,8865054:983040,0,0
x1,3:10292284,8865054
x1,3:12595150,8865054
x1,3:13032057,8865054
g1,3:13250510,8865054
x1,3:15070958,8865054
k1,4:31409438,8865054:16338480
g1,4:31409438,8865054
)
(1,6:8799518,9651486:22609920,455111,127431
h2,1:8799518,9651486:983040,0,0
x1,5:10110239,9651486
x1,5:13993253,9651486
)
]
]
}1
Input:3:/p/sec/a.tex
{2
[1,6:4736286,46220574:26673152,41484288,0
(3,1:8799518,8865054:22609920,447828,0
x3,1:10292284,8865054
)
]
}2
Postamble:
";

    fn parse(s: &str) -> SyncTex {
        SyncTex::parse(s.as_bytes(), |p| p.starts_with("/p/").then(|| PathBuf::from(p)))
    }

    #[test]
    fn units_match_synctex_view() {
        let st = parse(PDF);
        // `synctex view -i 3:1:main.tex`: h:133.768356 v:134.764618 H:6.807784
        let m = st.forward(Path::new("/p/main.tex"), 3, 0.0).unwrap();
        assert_eq!(m.page, 0);
        assert!((m.x - 133.768356).abs() < 1e-3, "{m:?}");
        assert!((m.y + m.height - 134.764618).abs() < 1e-3, "{m:?}");
        assert!((m.height - 6.807784).abs() < 1e-3, "{m:?}");
    }

    #[test]
    fn forward_moves_along_the_line_and_falls_back() {
        let st = parse(PDF);
        let start = st.forward(Path::new("/p/main.tex"), 3, 0.0).unwrap();
        let end = st.forward(Path::new("/p/main.tex"), 3, 0.99).unwrap();
        assert!(end.x > start.x);
        // Line 2 has no records: nothing before it either, so the next one.
        assert_eq!(st.forward(Path::new("/p/main.tex"), 2, 0.5).unwrap().x, start.x);
        // Second page, other file.
        assert_eq!(st.forward(Path::new("/p/sec/a.tex"), 1, 0.0).unwrap().page, 1);
        assert!(st.forward(Path::new("/p/other.tex"), 1, 0.0).is_none());
    }

    #[test]
    fn backward_finds_the_word() {
        let st = parse(PDF);
        // On the first line box, right of the 3rd record.
        let x = 13032057.0 / SP_PER_BP + 1.0;
        let y = 8865054.0 / SP_PER_BP - 2.0;
        let (path, line, frac) = st.backward(0, x, y).unwrap();
        assert_eq!(path, Path::new("/p/main.tex"));
        assert_eq!(line, 3);
        assert!(frac > 0.3 && frac < 0.9, "{frac}");
        // The second line box: the void box from the class is skipped.
        let (_, line, _) = st.backward(0, 200.0, 9651486.0 / SP_PER_BP - 1.0).unwrap();
        assert_eq!(line, 5);
        // Far below everything: the closest box.
        let (path, line, _) = st.backward(1, 150.0, 700.0).unwrap();
        assert_eq!((path.as_path(), line), (Path::new("/p/sec/a.tex"), 1));
    }

    #[test]
    fn dvi_offsets() {
        let dvi = "Input:1:/p/main.tex\nMagnification:1000\nUnit:1\nX Offset:4736287\nY Offset:4736287\nContent:\n{1\n(1,3:-4148,1621722:29694000,1310720,0\nx1,3:-4148,1621722\n)\n}1\n";
        let st = parse(dvi);
        let m = st.forward(Path::new("/p/main.tex"), 3, 0.0).unwrap();
        // `synctex view` on the upLaTeX run: h:71.936951
        assert!((m.x - 71.936951).abs() < 1e-3, "{m:?}");
    }
}
