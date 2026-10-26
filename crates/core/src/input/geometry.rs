//! Screen layout geometry (ADR-0006). Within a device the cursor moves in that device's
//! pixel space, as its OS would; between devices it moves through physical millimetres,
//! so an edge crossing keeps the physical height and the speed stays the same in mm/s.

use bridge_protocol::DeviceId;
use bridge_protocol::control::{LayoutUpdate, RectMm};

/// Edges of different devices closer than this are adjacent (snapping in the editor).
pub const SNAP_MM: f64 = 2.0;
/// Positions are half-open: `0 <= x < width`. The last representable pixel position.
const EDGE_EPS: f64 = 1e-6;
const MAX_HOPS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ScreenRef {
    pub device: DeviceId,
    pub screen: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub device: DeviceId,
    pub id: u32,
    pub px_w: u32,
    pub px_h: u32,
    /// Position in the device's virtual desktop, pixels.
    pub origin_x: i32,
    pub origin_y: i32,
    /// Placement in the shared layout.
    pub rect: RectMm,
}

impl Screen {
    pub fn r#ref(&self) -> ScreenRef {
        ScreenRef {
            device: self.device,
            screen: self.id,
        }
    }

    pub fn ppmm(&self) -> (f64, f64) {
        (
            f64::from(self.px_w) / self.rect.w,
            f64::from(self.px_h) / self.rect.h,
        )
    }

    pub fn to_mm(&self, x: f64, y: f64) -> (f64, f64) {
        let (px, py) = self.ppmm();
        (self.rect.x + x / px, self.rect.y + y / py)
    }

    pub fn from_mm(&self, mx: f64, my: f64) -> (f64, f64) {
        let (px, py) = self.ppmm();
        ((mx - self.rect.x) * px, (my - self.rect.y) * py)
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        x >= 0.0 && y >= 0.0 && x < f64::from(self.px_w) && y < f64::from(self.px_h)
    }

    fn contains_virtual(&self, vx: f64, vy: f64) -> bool {
        self.contains(vx - f64::from(self.origin_x), vy - f64::from(self.origin_y))
    }

    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(0.0, f64::from(self.px_w) - EDGE_EPS),
            y.clamp(0.0, f64::from(self.px_h) - EDGE_EPS),
        )
    }

    pub fn center(&self) -> (f64, f64) {
        (f64::from(self.px_w) / 2.0, f64::from(self.px_h) / 2.0)
    }
}

/// Cursor position: a screen and a (fractional) pixel on it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CursorPos {
    pub screen: ScreenRef,
    pub x: f64,
    pub y: f64,
}

impl CursorPos {
    /// Rounded pixel for injection.
    pub fn pixel(&self) -> (i32, i32) {
        (self.x.floor() as i32, self.y.floor() as i32)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    screens: Vec<Screen>,
}

impl Layout {
    pub fn new(screens: Vec<Screen>) -> Self {
        Self { screens }
    }

    pub fn from_update(update: &LayoutUpdate) -> Self {
        Self {
            screens: update
                .screens
                .iter()
                .map(|s| Screen {
                    device: s.device,
                    id: s.screen,
                    px_w: s.px_width,
                    px_h: s.px_height,
                    origin_x: s.origin_x,
                    origin_y: s.origin_y,
                    rect: s.rect_mm,
                })
                .collect(),
        }
    }

    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }

    pub fn screen(&self, r: ScreenRef) -> Option<&Screen> {
        self.screens
            .iter()
            .find(|s| s.device == r.device && s.id == r.screen)
    }

    pub fn device_screens(&self, device: DeviceId) -> impl Iterator<Item = &Screen> {
        self.screens.iter().filter(move |s| s.device == device)
    }

    /// Devices in reading order (left to right, then top to bottom), for "go to device N".
    pub fn devices_in_order(&self) -> Vec<DeviceId> {
        let mut firsts: Vec<(f64, f64, DeviceId)> = Vec::new();
        for s in &self.screens {
            if let Some(e) = firsts.iter_mut().find(|e| e.2 == s.device) {
                e.0 = e.0.min(s.rect.x);
                e.1 = e.1.min(s.rect.y);
            } else {
                firsts.push((s.rect.x, s.rect.y, s.device));
            }
        }
        firsts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        firsts.into_iter().map(|e| e.2).collect()
    }

    /// Moves the cursor by `(dx, dy)` pixels of its current screen. Crossings into
    /// another device only happen towards devices for which `present` is true; absent
    /// devices are skipped over (the layout collapses), otherwise the edge is a wall.
    pub fn step(
        &self,
        pos: CursorPos,
        dx: f64,
        dy: f64,
        present: &dyn Fn(&DeviceId) -> bool,
    ) -> CursorPos {
        let Some(mut screen) = self.screen(pos.screen) else {
            return pos;
        };
        if !(dx.is_finite() && dy.is_finite()) {
            return pos;
        }
        let (mut x, mut y, mut dx, mut dy) = (pos.x, pos.y, dx, dy);
        for _ in 0..MAX_HOPS {
            let (nx, ny) = (x + dx, y + dy);
            if screen.contains(nx, ny) {
                return CursorPos {
                    screen: screen.r#ref(),
                    x: nx,
                    y: ny,
                };
            }
            let Some((edge, t)) = exit(screen, x, y, dx, dy) else {
                let (cx, cy) = screen.clamp(nx, ny);
                return CursorPos {
                    screen: screen.r#ref(),
                    x: cx,
                    y: cy,
                };
            };
            let (ex, ey) = (x + dx * t, y + dy * t);
            let (rdx, rdy) = (dx * (1.0 - t), dy * (1.0 - t));
            if let Some((next, lx, ly)) = self.same_device_neighbour(screen, edge, ex, ey) {
                screen = next;
                (x, y, dx, dy) = (lx, ly, rdx, rdy);
                continue;
            }
            if let Some((next, lx, ly)) = self.other_device_neighbour(screen, edge, ex, ey, present)
            {
                let (sx, sy) = screen.ppmm();
                let (tx, ty) = next.ppmm();
                (x, y, dx, dy) = (lx, ly, rdx / sx * tx, rdy / sy * ty);
                screen = next;
                continue;
            }
            let (cx, cy) = screen.clamp(nx, ny);
            return CursorPos {
                screen: screen.r#ref(),
                x: cx,
                y: cy,
            };
        }
        let (cx, cy) = screen.clamp(x, y);
        CursorPos {
            screen: screen.r#ref(),
            x: cx,
            y: cy,
        }
    }

    fn same_device_neighbour(
        &self,
        from: &Screen,
        edge: Edge,
        ex: f64,
        ey: f64,
    ) -> Option<(&Screen, f64, f64)> {
        let (vx, vy) = (ex + f64::from(from.origin_x), ey + f64::from(from.origin_y));
        let (px, py) = nudge(edge, vx, vy, 0.5);
        let next = self
            .device_screens(from.device)
            .find(|s| s.id != from.id && s.contains_virtual(px, py))?;
        let (lx, ly) = (vx - f64::from(next.origin_x), vy - f64::from(next.origin_y));
        let (lx, ly) = next.clamp(lx, ly);
        Some((next, lx, ly))
    }

    fn other_device_neighbour(
        &self,
        from: &Screen,
        edge: Edge,
        ex: f64,
        ey: f64,
        present: &dyn Fn(&DeviceId) -> bool,
    ) -> Option<(&Screen, f64, f64)> {
        let (mut mx, mut my) = from.to_mm(ex, ey);
        let mut skip = vec![from.device];
        for _ in 0..MAX_HOPS {
            let next = self
                .screens
                .iter()
                .find(|s| !skip.contains(&s.device) && touches(s, edge, mx, my))?;
            if present(&next.device) {
                let (lx, ly) = next.from_mm(mx, my);
                let (lx, ly) = entry_side(next, edge, lx, ly);
                return Some((next, lx, ly));
            }
            // Absent device: continue on its far side, keeping the perpendicular coordinate.
            skip.push(next.device);
            match edge {
                Edge::Right => mx = next.rect.x + next.rect.w,
                Edge::Left => mx = next.rect.x,
                Edge::Bottom => my = next.rect.y + next.rect.h,
                Edge::Top => my = next.rect.y,
            }
        }
        None
    }
}

/// First boundary the segment `(x,y) + t·(dx,dy)` leaves through, with its `t` in `0..=1`.
fn exit(s: &Screen, x: f64, y: f64, dx: f64, dy: f64) -> Option<(Edge, f64)> {
    let (w, h) = (f64::from(s.px_w), f64::from(s.px_h));
    let mut best: Option<(Edge, f64)> = None;
    let mut consider = |edge, t: f64| {
        if (0.0..=1.0).contains(&t) && best.is_none_or(|(_, b)| t < b) {
            best = Some((edge, t));
        }
    };
    if dx > 0.0 && x + dx >= w {
        consider(Edge::Right, ((w - x) / dx).max(0.0));
    }
    if dx < 0.0 && x + dx < 0.0 {
        consider(Edge::Left, (x / -dx).max(0.0));
    }
    if dy > 0.0 && y + dy >= h {
        consider(Edge::Bottom, ((h - y) / dy).max(0.0));
    }
    if dy < 0.0 && y + dy < 0.0 {
        consider(Edge::Top, (y / -dy).max(0.0));
    }
    best
}

fn nudge(edge: Edge, x: f64, y: f64, by: f64) -> (f64, f64) {
    match edge {
        Edge::Right => (x + by, y),
        Edge::Left => (x - by, y),
        Edge::Bottom => (x, y + by),
        Edge::Top => (x, y - by),
    }
}

/// Is `s`'s near edge on the other side of `edge` at the millimetre point `(mx, my)`?
fn touches(s: &Screen, edge: Edge, mx: f64, my: f64) -> bool {
    let r = &s.rect;
    let along_y = my >= r.y && my < r.y + r.h;
    let along_x = mx >= r.x && mx < r.x + r.w;
    match edge {
        Edge::Right => along_y && (r.x - mx).abs() <= SNAP_MM,
        Edge::Left => along_y && (r.x + r.w - mx).abs() <= SNAP_MM,
        Edge::Bottom => along_x && (r.y - my).abs() <= SNAP_MM,
        Edge::Top => along_x && (r.y + r.h - my).abs() <= SNAP_MM,
    }
}

/// Places the entry point exactly on the near edge of the new screen.
fn entry_side(s: &Screen, edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let (w, h) = (f64::from(s.px_w), f64::from(s.px_h));
    let (x, y) = match edge {
        Edge::Right => (0.0, y),
        Edge::Left => (w - EDGE_EPS, y),
        Edge::Bottom => (x, 0.0),
        Edge::Top => (x, h - EDGE_EPS),
    };
    s.clamp(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    fn screen(
        d: u8,
        id: u32,
        px: (u32, u32),
        origin: (i32, i32),
        rect: (f64, f64, f64, f64),
    ) -> Screen {
        Screen {
            device: dev(d),
            id,
            px_w: px.0,
            px_h: px.1,
            origin_x: origin.0,
            origin_y: origin.1,
            rect: RectMm {
                x: rect.0,
                y: rect.1,
                w: rect.2,
                h: rect.3,
            },
        }
    }

    fn at(d: u8, id: u32, x: f64, y: f64) -> CursorPos {
        CursorPos {
            screen: ScreenRef {
                device: dev(d),
                screen: id,
            },
            x,
            y,
        }
    }

    fn all(_: &DeviceId) -> bool {
        true
    }

    /// Desktop (4K 27") on the left, laptop (1080p 14") to the right, lower down.
    fn desk_and_laptop() -> Layout {
        Layout::new(vec![
            screen(1, 0, (3840, 2160), (0, 0), (0.0, 0.0, 597.0, 336.0)),
            screen(2, 0, (1920, 1080), (0, 0), (597.0, 150.0, 310.0, 174.0)),
        ])
    }

    #[test]
    fn movement_inside_a_screen_is_plain_addition() {
        let l = desk_and_laptop();
        let p = l.step(at(1, 0, 100.0, 100.0), 10.5, -3.0, &all);
        assert_eq!(p, at(1, 0, 110.5, 97.0));
    }

    #[test]
    fn crossing_keeps_the_physical_height() {
        let l = desk_and_laptop();
        // 200 mm from the top of the desktop = 50 mm from the top of the laptop.
        let y_desk = 200.0 * 3840.0 / 597.0 * (2160.0 / 336.0) / (3840.0 / 597.0);
        let p = l.step(at(1, 0, 3839.5, y_desk), 1.0, 0.0, &all);
        assert_eq!(p.screen.device, dev(2));
        let (_, my) = l.screen(p.screen).unwrap().to_mm(p.x, p.y);
        assert!((my - 200.0).abs() < 1e-9, "{my}");
    }

    #[test]
    fn edge_without_neighbour_is_a_wall_and_sliding_works() {
        let l = desk_and_laptop();
        // Right edge of the desktop above the laptop's top: wall, but y still moves.
        let p = l.step(at(1, 0, 3839.0, 100.0), 50.0, 10.0, &all);
        assert_eq!(p.screen.device, dev(1));
        assert!(p.x < 3840.0 && p.x > 3839.0);
        assert!((p.y - 110.0).abs() < 1e-9);
        let p = l.step(at(1, 0, 5.0, 5.0), -100.0, -100.0, &all);
        assert_eq!(p.pixel(), (0, 0));
    }

    #[test]
    fn absent_device_is_a_wall_or_skipped() {
        let l = Layout::new(vec![
            screen(1, 0, (1920, 1080), (0, 0), (0.0, 0.0, 500.0, 280.0)),
            screen(2, 0, (1920, 1080), (0, 0), (500.0, 0.0, 500.0, 280.0)),
            screen(3, 0, (1920, 1080), (0, 0), (1000.0, 0.0, 500.0, 280.0)),
        ]);
        let only_1_and_3 = |d: &DeviceId| *d != dev(2);
        let p = l.step(at(1, 0, 1919.5, 540.0), 1.0, 0.0, &only_1_and_3);
        assert_eq!(
            p.screen.device,
            dev(3),
            "the layout collapses over the absent device"
        );
        let only_1 = |d: &DeviceId| *d == dev(1);
        let p = l.step(at(1, 0, 1919.5, 540.0), 1.0, 0.0, &only_1);
        assert_eq!(p.screen.device, dev(1), "no present device beyond: wall");
    }

    #[test]
    fn same_device_monitors_follow_the_os_pixel_arrangement() {
        // Two monitors of one device with different densities, arranged by the OS in pixels.
        let l = Layout::new(vec![
            screen(1, 0, (2560, 1440), (0, 0), (0.0, 0.0, 597.0, 336.0)),
            screen(1, 1, (1920, 1080), (2560, 0), (597.0, 0.0, 527.0, 296.0)),
        ]);
        let p = l.step(at(1, 0, 2559.5, 1000.0), 1.0, 0.0, &all);
        assert_eq!(
            p.screen,
            ScreenRef {
                device: dev(1),
                screen: 1
            }
        );
        assert!(
            (p.y - 1000.0).abs() < 1e-9,
            "same-device crossing keeps OS pixel rows"
        );
        assert!(l.step(at(1, 1, 10.0, 1070.0), 0.0, 20.0, &all).y < 1080.0);
    }

    #[test]
    fn vertical_crossing_and_order() {
        let l = Layout::new(vec![
            screen(1, 0, (1920, 1080), (0, 0), (0.0, 0.0, 500.0, 280.0)),
            screen(2, 0, (2560, 1600), (0, 0), (100.0, 281.0, 300.0, 187.0)),
        ]);
        let p = l.step(at(1, 0, 960.0, 1079.5), 0.0, 1.0, &all);
        assert_eq!(p.screen.device, dev(2), "snapping tolerates a 1 mm gap");
        // The remainder of the step was applied on the new screen; step back past it.
        let back = l.step(p, 0.0, -(p.y + 0.5), &all);
        assert_eq!(back.screen.device, dev(1));
        assert_eq!(l.devices_in_order(), vec![dev(1), dev(2)]);
    }

    #[test]
    fn non_finite_motion_is_ignored() {
        let l = desk_and_laptop();
        let p = at(1, 0, 10.0, 10.0);
        assert_eq!(l.step(p, f64::NAN, 0.0, &all), p);
        assert_eq!(l.step(p, f64::INFINITY, 1.0, &all), p);
        let unknown = at(9, 0, 1.0, 1.0);
        assert_eq!(l.step(unknown, 5.0, 5.0, &all), unknown);
    }

    fn side_by_side() -> impl Strategy<Value = (Layout, f64)> {
        (
            (100.0f64..800.0, 100.0f64..600.0, 640u32..7680, 480u32..4320),
            (100.0f64..800.0, 100.0f64..600.0, 640u32..7680, 480u32..4320),
            0.0f64..1.0,
            0.05f64..0.95,
        )
            .prop_map(|((wa, ha, pxa, pya), (wb, hb, pxb, pyb), shift, pick)| {
                // B's top is somewhere between A's top - B's height and A's bottom, so they overlap.
                let oy = -hb + (ha + hb) * shift;
                let lo = oy.max(0.0);
                let hi = (oy + hb).min(ha);
                let y_mm = lo + (hi - lo) * pick;
                let layout = Layout::new(vec![
                    screen(1, 0, (pxa, pya), (0, 0), (0.0, 0.0, wa, ha)),
                    screen(2, 0, (pxb, pyb), (0, 0), (wa, oy, wb, hb)),
                ]);
                (layout, y_mm)
            })
            .prop_filter("overlap must not be degenerate", |(l, y)| {
                let b = &l.screens()[1].rect;
                *y > b.y + 1e-3 && *y < b.y + b.h - 1e-3
            })
    }

    proptest! {
        #[test]
        fn crossing_and_back_returns_to_the_same_point((layout, y_mm) in side_by_side()) {
            let a = &layout.screens()[0];
            let (_, ya) = a.from_mm(0.0, y_mm);
            let start = at(1, 0, f64::from(a.px_w) - 0.25, ya);
            let there = layout.step(start, 0.5, 0.0, &all);
            prop_assert_eq!(there.screen.device, dev(2));
            let b = layout.screen(there.screen).unwrap();
            let (_, my) = b.to_mm(there.x, there.y);
            prop_assert!((my - y_mm).abs() < 1e-6, "entry height {} vs {}", my, y_mm);
            // Step back across the same edge.
            let back = layout.step(there, -(there.x + 0.25), 0.0, &all);
            prop_assert_eq!(back.screen.device, dev(1));
            prop_assert!((back.y - start.y).abs() < 1e-6, "y {} vs {}", back.y, start.y);
        }

        #[test]
        fn cursor_always_stays_on_a_screen(
            (layout, _) in side_by_side(),
            moves in proptest::collection::vec((-5000.0f64..5000.0, -5000.0f64..5000.0), 1..30),
        ) {
            let mut p = at(1, 0, 10.0, 10.0);
            for (dx, dy) in moves {
                p = layout.step(p, dx, dy, &all);
                let s = layout.screen(p.screen).unwrap();
                prop_assert!(p.x >= 0.0 && p.x < f64::from(s.px_w));
                prop_assert!(p.y >= 0.0 && p.y < f64::from(s.px_h));
            }
        }

        #[test]
        fn physical_speed_is_the_same_on_both_screens((layout, _) in side_by_side(), mm in 1.0f64..50.0) {
            // Moving `mm` millimetres expressed in each screen's pixels covers the same distance.
            for s in layout.screens() {
                let (ppx, _) = s.ppmm();
                let (x0, y0) = s.center();
                let p = layout.step(CursorPos { screen: s.r#ref(), x: x0, y: y0 }, mm * ppx * 0.1, 0.0, &all);
                let (m0, _) = s.to_mm(x0, y0);
                let (m1, _) = layout.screen(p.screen).unwrap().to_mm(p.x, p.y);
                prop_assert!(((m1 - m0) - mm * 0.1).abs() < 1e-6);
            }
        }
    }
}
