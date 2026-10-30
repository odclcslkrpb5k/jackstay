//! A machine's monitors, its sides' edges, and points on an edge as fractions of its covered
//! length (docs/input.md, "Layouts and edges").

use jackstay_proto::link::{MAX_MONITORS, Monitor, Side};

/// A fraction of an edge's covered length is in 1/65536.
const FRACTION_ONE: u64 = 1 << 16;

/// A rectangle in root-window pixels; never empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn right(self) -> i32 {
        self.x.saturating_add(self.width)
    }

    fn bottom(self) -> i32 {
        self.y.saturating_add(self.height)
    }

    #[must_use]
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// The point in this rectangle nearest to (`x`, `y`).
    #[must_use]
    pub fn clamp(self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.x, self.right().saturating_sub(1)),
            y.clamp(self.y, self.bottom().saturating_sub(1)),
        )
    }

    /// The squared distance from (`x`, `y`) to this rectangle; 0 inside it.
    fn distance(self, x: i32, y: i32) -> i64 {
        let (cx, cy) = self.clamp(x, y);
        let (dx, dy) = (
            i64::from(x).saturating_sub(i64::from(cx)),
            i64::from(y).saturating_sub(i64::from(cy)),
        );
        dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
    }
}

/// A machine's monitors: one to [`MAX_MONITORS`], each checked by [`Monitor::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    monitors: Vec<Rect>,
    bounds: Rect,
}

/// A layout with no monitors, too many, or one out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadLayout;

impl Layout {
    /// # Errors
    /// [`BadLayout`] if there is no monitor, more than [`MAX_MONITORS`], or one fails
    /// [`Monitor::check`].
    pub fn new(monitors: &[Monitor]) -> Result<Layout, BadLayout> {
        if monitors.is_empty() || monitors.len() > MAX_MONITORS {
            return Err(BadLayout);
        }
        if monitors.iter().any(|m| m.check().is_err()) {
            return Err(BadLayout);
        }
        let monitors: Vec<Rect> = monitors
            .iter()
            .map(|m| Rect {
                x: i32::from(m.x),
                y: i32::from(m.y),
                width: i32::from(m.width),
                height: i32::from(m.height),
            })
            .collect();
        let x = monitors.iter().map(|m| m.x).min().unwrap_or(0);
        let y = monitors.iter().map(|m| m.y).min().unwrap_or(0);
        let right = monitors.iter().map(|m| m.right()).max().unwrap_or(0);
        let bottom = monitors.iter().map(|m| m.bottom()).max().unwrap_or(0);
        let bounds =
            Rect { x, y, width: right.saturating_sub(x), height: bottom.saturating_sub(y) };
        Ok(Layout { monitors, bounds })
    }

    #[must_use]
    pub fn monitors(&self) -> &[Rect] {
        &self.monitors
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The monitor holding (`x`, `y`), if any.
    #[must_use]
    pub fn monitor_at(&self, x: i32, y: i32) -> Option<usize> {
        self.monitors.iter().position(|m| m.contains(x, y))
    }

    /// The monitor nearest to (`x`, `y`): the one holding it, if any.
    #[must_use]
    pub fn nearest(&self, x: i32, y: i32) -> usize {
        self.monitors.iter().enumerate().min_by_key(|(_, m)| m.distance(x, y)).map_or(0, |(i, _)| i)
    }

    /// Whether `monitor` lies on `side` of the bounding box.
    #[must_use]
    pub fn on_side(&self, monitor: usize, side: Side) -> bool {
        let (Some(m), b) = (self.monitors.get(monitor), self.bounds) else { return false };
        match side {
            Side::Left => m.x == b.x,
            Side::Right => m.right() == b.right(),
            Side::Top => m.y == b.y,
            Side::Bottom => m.bottom() == b.bottom(),
        }
    }

    /// The edge on `side`: the monitors' segments lying on it, merged where they touch or overlap.
    #[must_use]
    pub fn edge(&self, side: Side) -> Edge {
        let mut spans: Vec<(i32, i32)> = (0..self.monitors.len())
            .filter(|&i| self.on_side(i, side))
            .filter_map(|i| self.monitors.get(i))
            .map(|m| match side {
                Side::Left | Side::Right => (m.y, m.bottom()),
                Side::Top | Side::Bottom => (m.x, m.right()),
            })
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(i32, i32)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let b = self.bounds;
        // The line just inside the box on this side: where a pointer arriving through it lands.
        let across = match side {
            Side::Left => b.x,
            Side::Right => b.right().saturating_sub(1),
            Side::Top => b.y,
            Side::Bottom => b.bottom().saturating_sub(1),
        };
        Edge { side, across, spans: merged }
    }
}

/// The segments of one side's edge, in order along it, with gaps between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub side: Side,
    /// The coordinate across the edge of its outermost pixels (x for left and right).
    pub across: i32,
    /// `[start, end)` along the edge, sorted, disjoint, not touching.
    pub spans: Vec<(i32, i32)>,
}

impl Edge {
    /// The edge's covered length in pixels: the gaps between its segments are not counted.
    #[must_use]
    pub fn covered(&self) -> u64 {
        self.spans.iter().map(|(s, e)| u64::try_from(e.saturating_sub(*s)).unwrap_or(0)).sum()
    }

    /// The point `along` the edge (y for left and right), as a fraction of the covered length in
    /// 1/65536, gaps skipped. A point in a gap or beyond the ends counts as the nearest covered
    /// pixel. Exact: [`Edge::along`] gives back the same pixel for any edge of up to 65,536
    /// pixels.
    #[must_use]
    pub fn fraction(&self, along: i32) -> u16 {
        let covered = self.covered();
        if covered == 0 {
            return 0;
        }
        let mut before: u64 = 0;
        let mut offset = None;
        for &(start, end) in &self.spans {
            let len = u64::try_from(end.saturating_sub(start)).unwrap_or(0);
            if along < start {
                // In the gap before this segment (or before the first): its first pixel.
                offset = Some(before);
                break;
            }
            if along < end {
                offset = Some(
                    before.saturating_add(u64::try_from(along.saturating_sub(start)).unwrap_or(0)),
                );
                break;
            }
            before = before.saturating_add(len);
        }
        // Beyond the last segment: its last pixel.
        let offset = offset.unwrap_or_else(|| covered.saturating_sub(1));
        // Rounded up, so that rounding down on the way back gives the same pixel.
        let f = offset.saturating_mul(FRACTION_ONE).div_ceil(covered);
        u16::try_from(f).unwrap_or(u16::MAX)
    }

    /// The point along the edge at `fraction` of its covered length, gaps skipped; always on a
    /// segment.
    #[must_use]
    pub fn along(&self, fraction: u16) -> i32 {
        let covered = self.covered();
        let mut offset = u64::from(fraction).saturating_mul(covered) / FRACTION_ONE;
        for &(start, end) in &self.spans {
            let len = u64::try_from(end.saturating_sub(start)).unwrap_or(0);
            if offset < len {
                return start.saturating_add(i32::try_from(offset).unwrap_or(i32::MAX));
            }
            offset = offset.saturating_sub(len);
        }
        self.spans.last().map_or(0, |(_, end)| end.saturating_sub(1))
    }

    /// The pixel just inside the edge at `fraction`, as (x, y).
    #[must_use]
    pub fn point(&self, fraction: u16) -> (i32, i32) {
        let along = self.along(fraction);
        match self.side {
            Side::Left | Side::Right => (self.across, along),
            Side::Top | Side::Bottom => (along, self.across),
        }
    }
}
