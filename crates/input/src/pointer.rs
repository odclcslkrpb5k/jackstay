//! The secondary's virtual pointer (docs/input.md, "Secondary"): moved by the primary's deltas,
//! kept on a monitor, and leaving through a side with a neighbour.

use jackstay_proto::link::Side;

use crate::layout::Layout;

/// Positions are kept in 1/256 px, so that fractions of a pixel are carried, not rounded away.
const SUB: i64 = 256;

/// The sides with a neighbour, as a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sides(u8);

impl Sides {
    fn bit(side: Side) -> u8 {
        match side {
            Side::Left => 1,
            Side::Right => 2,
            Side::Top => 4,
            Side::Bottom => 8,
        }
    }

    #[must_use]
    pub fn with(self, side: Side) -> Sides {
        Sides(self.0 | Sides::bit(side))
    }

    #[must_use]
    pub fn has(self, side: Side) -> bool {
        self.0 & Sides::bit(side) != 0
    }
}

/// What a move did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The pointer is now at this pixel (perhaps the one it was at).
    At { x: i32, y: i32 },
    /// The pointer left through `side` at `fraction` of that edge; it stays at its last pixel.
    Left { side: Side, fraction: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// In 1/256 px.
    x: i64,
    y: i64,
    monitor: usize,
}

fn pixel(sub: i64) -> i32 {
    i32::try_from(sub.div_euclid(SUB)).unwrap_or(if sub < 0 { i32::MIN } else { i32::MAX })
}

fn sub(pixel: i32) -> i64 {
    i64::from(pixel).saturating_mul(SUB)
}

impl Pointer {
    /// The pointer entering `layout` through `side` at `fraction` of its edge.
    #[must_use]
    pub fn enter(layout: &Layout, side: Side, fraction: u16) -> Pointer {
        let (x, y) = layout.edge(side).point(fraction);
        Pointer::at(layout, x, y)
    }

    /// The pointer at pixel (`x`, `y`), or the nearest pixel on a monitor.
    #[must_use]
    pub fn at(layout: &Layout, x: i32, y: i32) -> Pointer {
        let monitor = layout.nearest(x, y);
        let (x, y) = layout.monitors().get(monitor).map_or((x, y), |m| m.clamp(x, y));
        // The middle of the pixel, so small motion either way stays in it.
        Pointer { x: sub(x).saturating_add(SUB / 2), y: sub(y).saturating_add(SUB / 2), monitor }
    }

    #[must_use]
    pub fn pixel(&self) -> (i32, i32) {
        (pixel(self.x), pixel(self.y))
    }

    /// Kept on a monitor of a new layout: where it was, if that is still on one, and otherwise
    /// the nearest pixel that is.
    pub fn relayout(&mut self, layout: &Layout) {
        let (x, y) = self.pixel();
        match layout.monitor_at(x, y) {
            Some(m) => self.monitor = m,
            None => *self = Pointer::at(layout, x, y),
        }
    }

    /// Moves by (`dx`, `dy`) in 1/256 px. A move that would leave every monitor stops at the edge
    /// of the monitor the pointer is on; if that edge is on a side in `exits`, and `may_leave`,
    /// the pointer leaves through it instead.
    pub fn move_by(
        &mut self,
        layout: &Layout,
        dx: i32,
        dy: i32,
        exits: Sides,
        may_leave: bool,
    ) -> Step {
        let (tx, ty) = (self.x.saturating_add(i64::from(dx)), self.y.saturating_add(i64::from(dy)));
        let (px, py) = (pixel(tx), pixel(ty));
        if let Some(m) = layout.monitor_at(px, py) {
            (self.x, self.y, self.monitor) = (tx, ty, m);
            return Step::At { x: px, y: py };
        }
        // A long move can pass through other monitors on its way off them all: the one holding
        // the target clamped into the bounding box is the one it reached, if any.
        let (bx, by) = layout.bounds().clamp(px, py);
        if let Some(reached) = layout.monitor_at(bx, by) {
            self.monitor = reached;
        }
        let Some(m) = layout.monitors().get(self.monitor).copied() else {
            *self = Pointer::at(layout, px, py);
            let (x, y) = self.pixel();
            return Step::At { x, y };
        };
        let (cx, cy) = m.clamp(px, py);
        // How far past each side of this monitor the move went, in pixels.
        let over = |side: Side| -> i64 {
            let d = match side {
                Side::Left => cx.saturating_sub(px),
                Side::Right => px.saturating_sub(cx),
                Side::Top => cy.saturating_sub(py),
                Side::Bottom => py.saturating_sub(cy),
            };
            i64::from(d)
        };
        let leaving = Side::ALL
            .into_iter()
            .filter(|&s| over(s) > 0 && exits.has(s) && layout.on_side(self.monitor, s))
            .max_by_key(|&s| over(s));
        // Clamped to this monitor, keeping the fraction of a pixel on the axis that did not stop.
        let keep = |t: i64, c: i32, p: i32| if c == p { t } else { sub(c).saturating_add(SUB / 2) };
        self.x = keep(tx, cx, px);
        self.y = keep(ty, cy, py);
        match leaving {
            Some(side) if may_leave => {
                let along = match side {
                    Side::Left | Side::Right => cy,
                    Side::Top | Side::Bottom => cx,
                };
                Step::Left { side, fraction: layout.edge(side).fraction(along) }
            }
            _ => Step::At { x: cx, y: cy },
        }
    }
}
