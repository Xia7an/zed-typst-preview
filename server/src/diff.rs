//! Layout diffing for incremental rendering.
//!
//! Typing usually changes a line or two of one page. Comparing the old and
//! the new layout of that page tells which horizontal bands look different,
//! so only those need new pixels. The same bounding boxes are used to skip
//! items outside the band being rasterized ([`cull`]).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use typst::layout::{Frame, FrameItem, GroupItem, Point, Transform};
use typst::text::TextItem;
use typst::utils::hash128;
use typst_layout::Page;

/// A horizontal band of a page in pt: `(top, bottom)`.
pub type Band = (f64, f64);

/// Extra space around changed items (anti-aliasing, rounding), in pt.
const PAD: f64 = 1.0;

/// An axis-aligned box in pt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    pub const EMPTY: Self = Self { x0: f64::INFINITY, y0: f64::INFINITY, x1: f64::NEG_INFINITY, y1: f64::NEG_INFINITY };

    fn new(min: Point, max: Point) -> Self {
        Self { x0: min.x.to_pt(), y0: min.y.to_pt(), x1: max.x.to_pt(), y1: max.y.to_pt() }
    }

    pub fn is_empty(&self) -> bool {
        !(self.x0 <= self.x1 && self.y0 <= self.y1)
    }

    fn union(self, o: Self) -> Self {
        Self { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    fn translate(self, p: Point) -> Self {
        let (dx, dy) = (p.x.to_pt(), p.y.to_pt());
        Self { x0: self.x0 + dx, y0: self.y0 + dy, x1: self.x1 + dx, y1: self.y1 + dy }
    }

    /// The bounding box of this box after transforming it.
    fn transform(self, ts: Transform) -> Self {
        if self.is_empty() || ts == Transform::identity() {
            return self;
        }
        let corners = [(self.x0, self.y0), (self.x1, self.y0), (self.x0, self.y1), (self.x1, self.y1)];
        corners.iter().fold(Self::EMPTY, |acc, &(x, y)| {
            let p = Point::new(typst::layout::Abs::pt(x), typst::layout::Abs::pt(y)).transform(ts);
            acc.union(Self::new(p, p))
        })
    }
}

/// Where `item`, placed at `pos`, may paint (conservative).
pub fn item_bbox(pos: Point, item: &FrameItem) -> BBox {
    match item {
        FrameItem::Group(group) => group_bbox(group).transform(Transform::translate(pos.x, pos.y)),
        FrameItem::Text(text) => text_bbox(text).translate(pos),
        FrameItem::Shape(shape, _) => {
            let r = shape.bbox(true);
            BBox::new(r.min, r.max).translate(pos)
        }
        FrameItem::Image(_, size, _) => BBox::new(Point::zero(), size.to_point()).translate(pos),
        FrameItem::Link(..) | FrameItem::Tag(_) => BBox::EMPTY,
    }
}

fn frame_bbox(frame: &Frame) -> BBox {
    frame.items().fold(BBox::EMPTY, |acc, (pos, item)| acc.union(item_bbox(*pos, item)))
}

fn group_bbox(group: &GroupItem) -> BBox {
    let inner = match &group.clip {
        Some(clip) => {
            let r = clip.bbox(None);
            BBox::new(r.min, r.max)
        }
        None => frame_bbox(&group.frame),
    };
    inner.transform(group.transform)
}

fn text_bbox(text: &TextItem) -> BBox {
    let size = text.size.to_pt();
    // The font's global box covers all of its glyphs (cheap, no outlines).
    let font = &text.font;
    let upem = font.units_per_em();
    let bb = font.ttf().global_bounding_box();
    let em = |v: i16| v as f64 / upem * size;
    let (mut x, mut y, mut ox, mut oy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut xmin, mut xmax, mut ymin, mut ymax) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for g in &text.glyphs {
        ox = ox.max(g.x_offset.at(text.size).to_pt().abs());
        oy = oy.max(g.y_offset.at(text.size).to_pt().abs());
        x += g.x_advance.at(text.size).to_pt();
        y += g.y_advance.at(text.size).to_pt();
        xmin = xmin.min(x);
        xmax = xmax.max(x);
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    let stroke = text.stroke.as_ref().map_or(0.0, |s| s.thickness.to_pt());
    let pad = ox.max(oy) + stroke;
    // Glyph y is up in font units, down on the page.
    BBox {
        x0: xmin + em(bb.x_min).min(0.0) - pad,
        x1: xmax + em(bb.x_max).max(0.0) + pad,
        y0: -ymax - em(bb.y_max).max(0.0) - pad,
        y1: -ymin - em(bb.y_min).min(0.0) + pad,
    }
}

/// Hashes what an item looks like (ignoring source spans and tags, which
/// change while typing without changing any pixels).
fn visual_hash(item: &FrameItem) -> Option<u128> {
    Some(match item {
        FrameItem::Group(group) => {
            let items: Vec<(Point, u128)> =
                group.frame.items().filter_map(|(p, i)| Some((*p, visual_hash(i)?))).collect();
            hash128(&(0u8, group.transform, &group.clip, group.frame.kind(), group.frame.size(), items))
        }
        FrameItem::Text(text) => hash128(&VisualText(text)),
        FrameItem::Shape(shape, _) => hash128(&(2u8, shape)),
        FrameItem::Image(image, size, _) => hash128(&(3u8, image, size)),
        FrameItem::Link(..) | FrameItem::Tag(_) => return None,
    })
}

struct VisualText<'a>(&'a TextItem);

impl Hash for VisualText<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let t = self.0;
        1u8.hash(state);
        t.font.hash(state);
        t.size.hash(state);
        t.fill.hash(state);
        t.stroke.hash(state);
        t.glyphs.len().hash(state);
        for g in &t.glyphs {
            (g.id, g.x_advance, g.x_offset, g.y_advance, g.y_offset).hash(state);
        }
    }
}

/// Bands of `new` that may look different from `old`, or `None` if the whole
/// page must be redrawn (size or background changed).
pub fn dirty_bands(old: &Page, new: &Page) -> Option<Vec<Band>> {
    if old.frame.size() != new.frame.size() || old.fill != new.fill {
        return None;
    }
    let mut boxes = Vec::new();
    diff_frames(&old.frame, &new.frame, Transform::identity(), &mut boxes);
    let height = new.frame.height().to_pt();
    let mut bands: Vec<Band> = boxes
        .iter()
        .filter(|b| !b.is_empty())
        .map(|b| ((b.y0 - PAD).max(0.0), (b.y1 + PAD).min(height)))
        .filter(|(a, b)| a < b)
        .collect();
    bands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<Band> = Vec::with_capacity(bands.len());
    for (a, b) in bands {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    Some(merged)
}

/// Collects the boxes (in page coordinates) of items that differ between
/// two frames. Groups placed identically in both are compared recursively,
/// so an edit inside a paragraph only marks the lines that changed.
fn diff_frames(old: &Frame, new: &Frame, ts: Transform, out: &mut Vec<BBox>) {
    let old_items: Vec<&(Point, FrameItem)> = old.items().collect();
    let key = |(pos, item): &(Point, FrameItem)| visual_hash(item).map(|h| hash128(&(pos, h)));

    let mut by_key: HashMap<u128, Vec<usize>> = HashMap::new();
    for (i, entry) in old_items.iter().enumerate() {
        if let Some(k) = key(entry) {
            by_key.entry(k).or_default().push(i);
        }
    }
    let mut added = Vec::new();
    for entry in new.items() {
        let Some(k) = key(entry) else { continue };
        if by_key.get_mut(&k).and_then(Vec::pop).is_none() {
            added.push(entry);
        }
    }
    let mut removed: Vec<usize> = by_key.into_values().flatten().collect();
    removed.sort_unstable();

    for (pos, item) in added {
        if let FrameItem::Group(group) = item {
            let twin = removed.iter().position(|&i| match old_items[i] {
                (p, FrameItem::Group(g)) => {
                    p == pos
                        && g.transform == group.transform
                        && g.clip == group.clip
                        && g.frame.size() == group.frame.size()
                        && g.frame.kind() == group.frame.kind()
                }
                _ => false,
            });
            if let Some(t) = twin {
                let FrameItem::Group(old_group) = &old_items[removed.remove(t)].1 else { unreachable!() };
                let child = ts.pre_concat(Transform::translate(pos.x, pos.y)).pre_concat(group.transform);
                diff_frames(&old_group.frame, &group.frame, child, out);
                continue;
            }
        }
        out.push(item_bbox(*pos, item).transform(ts));
    }
    for i in removed {
        let (pos, item) = old_items[i];
        out.push(item_bbox(*pos, item).transform(ts));
    }
}

/// A copy of `frame` without the items that cannot paint inside the band
/// `y0..y1` (pt). Untransformed groups are culled recursively.
pub fn cull(frame: &Frame, y0: f64, y1: f64) -> Frame {
    let mut out = Frame::new(frame.size(), frame.kind());
    for (pos, item) in frame.items() {
        let b = item_bbox(*pos, item);
        if b.is_empty() || b.y1 < y0 || b.y0 > y1 {
            continue;
        }
        match item {
            FrameItem::Group(group)
                if group.transform == Transform::identity() && (b.y0 < y0 || b.y1 > y1) =>
            {
                let dy = pos.y.to_pt();
                let mut g = group.clone();
                g.frame = cull(&group.frame, y0 - dy, y1 - dy);
                out.push(*pos, FrameItem::Group(g));
            }
            _ => out.push(*pos, item.clone()),
        }
    }
    out
}

/// Strips (of `strip_px` rows, at `scale` px/pt) touched by `bands`, for a
/// page `height_px` tall. Mirrors `dirty_strips` in Zed's preview pane.
pub fn dirty_strips(bands: &[Band], scale: f64, height_px: u32, strip_px: u32) -> Vec<u32> {
    if height_px == 0 || strip_px == 0 {
        return Vec::new();
    }
    let last = (height_px - 1) / strip_px;
    let mut out = Vec::new();
    for &(y0, y1) in bands {
        let r0 = ((y0 * scale).floor() - 1.0).max(0.0) as u32;
        let r1 = ((y1 * scale).ceil() + 1.0).max(0.0) as u32;
        out.extend((r0 / strip_px).min(last)..=(r1 / strip_px).min(last));
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_of_bands() {
        // 100 px/pt is easy to read: rows = pt * 100.
        assert_eq!(dirty_strips(&[(1.0, 2.0)], 100.0, 1000, 128), vec![0, 1]);
        assert_eq!(dirty_strips(&[(3.0, 3.5)], 100.0, 1000, 128), vec![2]);
        assert_eq!(dirty_strips(&[(9.0, 20.0)], 100.0, 1000, 128), vec![7]);
        assert!(dirty_strips(&[], 100.0, 1000, 128).is_empty());
    }
}
