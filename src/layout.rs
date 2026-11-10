use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A split's dividing line, picked up with the mouse to resize it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divider {
    /// The way to the split from the top of the layout: `true` for second.
    path: Vec<bool>,
    axis: Axis,
    /// The room the split shares out.
    area: Rect,
    /// Where on the line it was picked up: the first pane's border (-1) or the
    /// second's (0), so it doesn't jump a cell when dragged.
    grip: i32,
}

/// The two parts of `rect` a split gives its panes.
fn halves(axis: Axis, percent: u16, rect: Rect) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let w = (rect.width as u32 * percent as u32 / 100) as u16;
            (
                Rect::new(rect.x, rect.y, w, rect.height),
                Rect::new(rect.x + w, rect.y, rect.width - w, rect.height),
            )
        }
        Axis::Vertical => {
            let h = (rect.height as u32 * percent as u32 / 100) as u16;
            (
                Rect::new(rect.x, rect.y, rect.width, h),
                Rect::new(rect.x, rect.y + h, rect.width, rect.height - h),
            )
        }
    }
}

/// The narrowest share a split leaves either side, by keys or by the mouse.
const SMALLEST: u16 = 15;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Tile {
    Leaf(usize),
    Split {
        axis: Axis,
        percent: u16,
        first: Box<Tile>,
        second: Box<Tile>,
    },
}

impl Tile {
    #[cfg(test)]
    pub fn rectangles(&self, rect: Rect, out: &mut Vec<(usize, Rect)>) {
        self.layout(rect, &[], out);
    }

    /// Like [`rectangles`](Self::rectangles), leaving out the `hidden` (docked)
    /// panes: the rest of their split takes their room, and they come back to
    /// the same place and size.
    pub fn layout(&self, rect: Rect, hidden: &[usize], out: &mut Vec<(usize, Rect)>) {
        match self {
            Self::Leaf(id) => {
                if !hidden.contains(id) {
                    out.push((*id, rect));
                }
            }
            Self::Split {
                axis,
                percent,
                first,
                second,
            } => {
                match (first.shown(hidden), second.shown(hidden)) {
                    (true, false) => return first.layout(rect, hidden, out),
                    (false, true) => return second.layout(rect, hidden, out),
                    (false, false) => return,
                    (true, true) => {}
                }
                let (a, b) = halves(*axis, *percent, rect);
                first.layout(a, hidden, out);
                second.layout(b, hidden, out);
            }
        }
    }

    /// The divider under `at` in the layout drawn in `rect`: the borders of the two
    /// panes either side of a split, the whole length of the split. Where lines
    /// cross, the longer one: the outer split.
    pub fn divider_at(&self, rect: Rect, hidden: &[usize], at: (u16, u16)) -> Option<Divider> {
        let mut path = Vec::new();
        let mut tile = self;
        let mut rect = rect;
        let inside = |r: Rect| at.0 >= r.x && at.0 < r.right() && at.1 >= r.y && at.1 < r.bottom();
        loop {
            let Self::Split {
                axis,
                percent,
                first,
                second,
            } = tile
            else {
                return None;
            };
            // A side with every pane docked gives the other all the room.
            match (first.shown(hidden), second.shown(hidden)) {
                (true, false) => {
                    path.push(false);
                    tile = first;
                    continue;
                }
                (false, true) => {
                    path.push(true);
                    tile = second;
                    continue;
                }
                (false, false) => return None,
                (true, true) => {}
            }
            let (a, b) = halves(*axis, *percent, rect);
            let (along, line) = match axis {
                Axis::Horizontal => (at.0, b.x),
                Axis::Vertical => (at.1, b.y),
            };
            if inside(rect) && (along + 1 == line || along == line) {
                return Some(Divider {
                    path,
                    axis: *axis,
                    area: rect,
                    grip: i32::from(along) - i32::from(line),
                });
            }
            let next = inside(b);
            path.push(next);
            (tile, rect) = if next { (second, b) } else { (first, a) };
        }
    }

    /// Move `divider` so the line follows the pointer at `at`; false when it no
    /// longer exists or doesn't move.
    pub fn drag(&mut self, divider: &Divider, at: (u16, u16)) -> bool {
        let mut tile = self;
        for &second in &divider.path {
            let Self::Split {
                first, second: s, ..
            } = tile
            else {
                return false;
            };
            tile = if second { s } else { first };
        }
        let Self::Split { axis, percent, .. } = tile else {
            return false;
        };
        if *axis != divider.axis {
            return false;
        }
        let (start, size, along) = match axis {
            Axis::Horizontal => (divider.area.x, divider.area.width, at.0),
            Axis::Vertical => (divider.area.y, divider.area.height, at.1),
        };
        if size == 0 {
            return false;
        }
        let first = i32::from(along) - divider.grip - i32::from(start);
        let wanted = (first * 100 + i32::from(size) / 2) / i32::from(size);
        let wanted = wanted.clamp(i32::from(SMALLEST), i32::from(100 - SMALLEST)) as u16;
        let moved = wanted != *percent;
        *percent = wanted;
        moved
    }

    pub fn split(&mut self, target: usize, new: usize, axis: Axis) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    percent: 50,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            }
        }
    }

    pub fn remove(&mut self, target: usize) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if matches!(**first, Self::Leaf(id) if id == target) {
                    *self = (**second).clone();
                    true
                } else if matches!(**second, Self::Leaf(id) if id == target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove(target) || second.remove(target)
                }
            }
        }
    }

    pub fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    pub fn contains(&self, target: usize) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    /// Whether any pane in this part of the layout is not hidden.
    fn shown(&self, hidden: &[usize]) -> bool {
        match self {
            Self::Leaf(id) => !hidden.contains(id),
            Self::Split { first, second, .. } => first.shown(hidden) || second.shown(hidden),
        }
    }

    /// Exchange two panes' places.
    pub fn swap(&mut self, a: usize, b: usize) {
        match self {
            Self::Leaf(id) if *id == a => *id = b,
            Self::Leaf(id) if *id == b => *id = a,
            Self::Leaf(_) => {}
            Self::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    /// Turn the split holding `target` between side by side and stacked.
    pub fn flip(&mut self, target: usize) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                if first.flip(target) || second.flip(target) {
                    return true;
                }
                let holds = |t: &Tile| matches!(t, Self::Leaf(id) if *id == target);
                if holds(first) || holds(second) {
                    *axis = match axis {
                        Axis::Horizontal => Axis::Vertical,
                        Axis::Vertical => Axis::Horizontal,
                    };
                    return true;
                }
                false
            }
        }
    }

    /// Move `target` out to one whole side of the layout (`last`: right or bottom),
    /// the rest keeping the other side. False when it's already there, or alone.
    pub fn move_to_side(&mut self, target: usize, axis: Axis, last: bool) -> bool {
        if let Self::Split {
            axis: a,
            first,
            second,
            ..
        } = self
        {
            let edge = if last { second } else { first };
            if *a == axis && matches!(**edge, Self::Leaf(id) if id == target) {
                return false;
            }
        }
        if !self.remove(target) {
            return false;
        }
        let rest = Box::new(std::mem::replace(self, Self::Leaf(target)));
        let moved = Box::new(Self::Leaf(target));
        let (first, second) = if last { (rest, moved) } else { (moved, rest) };
        *self = Self::Split {
            axis,
            percent: 50,
            first,
            second,
        };
        true
    }

    pub fn resize(&mut self, target: usize, axis: Axis, delta: i16) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                axis: split_axis,
                percent,
                first,
                second,
            } => {
                if first.resize(target, axis, delta) || second.resize(target, axis, delta) {
                    return true;
                }
                if *split_axis == axis && (first.contains(target) || second.contains(target)) {
                    *percent = (*percent as i16 + delta)
                        .clamp(SMALLEST as i16, 100 - SMALLEST as i16)
                        as u16;
                    return true;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three panes stacked on the left, one the whole height on the right.
    #[test]
    fn dragging_a_divider_resizes_everything_along_it() {
        let mut tile = Tile::Leaf(0);
        tile.split(0, 3, Axis::Horizontal);
        tile.split(0, 1, Axis::Vertical);
        tile.split(1, 2, Axis::Vertical);
        let area = Rect::new(10, 2, 100, 40);
        let rects = |tile: &Tile| {
            let mut out = Vec::new();
            tile.rectangles(area, &mut out);
            out
        };
        let before = rects(&tile);
        let right = before.iter().find(|(id, _)| *id == 3).unwrap().1;
        assert_eq!(right.x, 60);
        // The line between the stack and the right pane, on either border and
        // at any height, even where the stack's own lines meet it.
        for at in [(59, 2), (60, 20), (59, 41), (60, before[1].1.y)] {
            let divider = tile.divider_at(area, &[], at).unwrap();
            assert_eq!((divider.path.len(), divider.axis), (0, Axis::Horizontal));
        }
        assert!(tile.divider_at(area, &[], (58, 20)).is_none());
        assert!(
            tile.divider_at(area, &[], (10, 20)).is_none(),
            "the outer edge"
        );
        // Dragged left by 20 columns: the right pane grows, the whole stack shrinks.
        let divider = tile.divider_at(area, &[], (59, 30)).unwrap();
        assert!(tile.drag(&divider, (39, 30)));
        for (id, rect) in rects(&tile) {
            if id == 3 {
                assert_eq!((rect.x, rect.width), (40, 70));
            } else {
                assert_eq!((rect.x, rect.width), (10, 30), "pane {id}");
            }
        }
        // As far as the smallest share, and no further.
        assert!(tile.drag(&divider, (0, 30)));
        assert_eq!(rects(&tile)[0].1.width, 15);
        assert!(!tile.drag(&divider, (1, 30)));
        // A line inside the stack moves only the panes either side of it.
        let before = rects(&tile);
        let line = before[1].1.y;
        let divider = tile.divider_at(area, &[], (12, line)).unwrap();
        assert_eq!(
            (divider.path.as_slice(), divider.axis),
            (&[false][..], Axis::Vertical)
        );
        assert!(tile.drag(&divider, (12, line + 4)));
        let after = rects(&tile);
        assert_eq!(after[0].1.height, before[0].1.height + 4);
        assert_eq!(after[1].1.y, line + 4);
        // The two below share what is left; the right pane stays as it was.
        assert_eq!(after[2].1.bottom(), before[2].1.bottom());
        assert_eq!(after[3].1, before[3].1);
        // With the right pane docked there is no line to pick up there.
        assert!(tile.divider_at(area, &[3], (24, 20)).is_none());
    }
    #[test]
    fn nested_splits_cover_area_and_collapse() {
        let mut tile = Tile::Leaf(0);
        tile.split(0, 1, Axis::Horizontal);
        tile.split(1, 2, Axis::Vertical);
        let mut rects = Vec::new();
        tile.rectangles(Rect::new(0, 0, 101, 41), &mut rects);
        assert_eq!(
            rects
                .iter()
                .map(|(_, r)| r.width as u32 * r.height as u32)
                .sum::<u32>(),
            101 * 41
        );
        assert!(tile.remove(1));
        assert!(!tile.contains(1));
        assert!(tile.contains(0) && tile.contains(2));
    }
}
