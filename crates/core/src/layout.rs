//! Shared screen layout (ADR-0006): every device writes only its own screens into the
//! common arrangement; the highest Lamport version wins.

use bridge_protocol::DeviceId;
use bridge_protocol::control::{LayoutUpdate, MAX_SCREENS, RectMm, ScreenPlacement};

/// A monitor as the local OS reports it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LocalScreen {
    pub id: u32,
    pub px_w: u32,
    pub px_h: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    /// OS scale factor × 1000 (1000 = 100 %).
    pub scale_permille: u32,
    /// Physical size from EDID, when the OS knows it.
    pub size_mm: Option<(f64, f64)>,
}

impl LocalScreen {
    /// Physical size; without EDID assume 96 dpi at 100 % scale, as OSes do.
    pub fn size_mm(&self) -> (f64, f64) {
        if let Some((w, h)) = self.size_mm.filter(|(w, h)| *w > 1.0 && *h > 1.0) {
            return (w, h);
        }
        let dpi = 96.0 * f64::from(self.scale_permille.clamp(250, 8000)) / 1000.0;
        let mm = |px: u32| f64::from(px) / dpi * 25.4;
        (mm(self.px_w), mm(self.px_h))
    }
}

/// Places `screens` (this device's monitors) into `current`, keeping positions the user
/// already set and putting a new device to the right of everything else.
pub(crate) fn with_own_screens(
    current: Option<&LayoutUpdate>,
    me: DeviceId,
    screens: &[LocalScreen],
) -> Vec<ScreenPlacement> {
    let all: Vec<ScreenPlacement> = current.map(|l| l.screens.to_vec()).unwrap_or_default();
    let (own, mut others): (Vec<_>, Vec<_>) = all.into_iter().partition(|s| s.device == me);

    let anchor = own
        .iter()
        .find(|s| screens.iter().any(|l| l.id == s.screen))
        .map(|s| {
            let ppmm_x = f64::from(s.px_width) / s.rect_mm.w;
            let ppmm_y = f64::from(s.px_height) / s.rect_mm.h;
            (
                s.rect_mm.x - f64::from(s.origin_x) / ppmm_x,
                s.rect_mm.y - f64::from(s.origin_y) / ppmm_y,
            )
        })
        .unwrap_or_else(|| {
            let right = others
                .iter()
                .map(|s| s.rect_mm.x + s.rect_mm.w)
                .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))));
            let top = others
                .iter()
                .map(|s| s.rect_mm.y)
                .fold(None, |m: Option<f64>, y| Some(m.map_or(y, |m| m.min(y))));
            (right.unwrap_or(0.0), top.unwrap_or(0.0))
        });

    let mut mine: Vec<ScreenPlacement> = screens
        .iter()
        .take(MAX_SCREENS)
        .map(|l| {
            let scale = l.scale_permille.clamp(250, 8000);
            if let Some(prev) = own.iter().find(|s| {
                s.screen == l.id
                    && s.px_width == l.px_w
                    && s.px_height == l.px_h
                    && s.scale_permille == scale
            }) {
                let mut p = prev.clone();
                p.origin_x = l.origin_x;
                p.origin_y = l.origin_y;
                return p;
            }
            let (w, h) = l.size_mm();
            let (ppmm_x, ppmm_y) = (f64::from(l.px_w) / w, f64::from(l.px_h) / h);
            ScreenPlacement {
                device: me,
                screen: l.id,
                px_width: l.px_w.clamp(1, 65_535),
                px_height: l.px_h.clamp(1, 65_535),
                origin_x: l.origin_x,
                origin_y: l.origin_y,
                scale_permille: scale,
                rect_mm: RectMm {
                    x: anchor.0 + f64::from(l.origin_x) / ppmm_x,
                    y: anchor.1 + f64::from(l.origin_y) / ppmm_y,
                    w: w.clamp(0.01, 10_000.0),
                    h: h.clamp(0.01, 10_000.0),
                },
            }
        })
        .collect();
    others.truncate(MAX_SCREENS.saturating_sub(mine.len()));
    others.append(&mut mine);
    others.sort_by(|a, b| a.device.cmp(&b.device).then(a.screen.cmp(&b.screen)));
    others
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_protocol::BoundedVec;
    use bridge_protocol::control::LayoutVersion;

    fn local(id: u32, px: (u32, u32), origin: (i32, i32)) -> LocalScreen {
        LocalScreen {
            id,
            px_w: px.0,
            px_h: px.1,
            origin_x: origin.0,
            origin_y: origin.1,
            scale_permille: 1000,
            size_mm: None,
        }
    }

    fn update(screens: Vec<ScreenPlacement>) -> LayoutUpdate {
        LayoutUpdate {
            version: LayoutVersion {
                counter: 1,
                device: DeviceId([0; 32]),
            },
            screens: BoundedVec::try_from_vec(screens).unwrap(),
        }
    }

    #[test]
    fn first_device_starts_at_origin_and_second_goes_to_the_right() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        let la = with_own_screens(
            None,
            a,
            &[
                local(0, (1920, 1080), (0, 0)),
                local(1, (1920, 1080), (1920, 0)),
            ],
        );
        assert_eq!(la.len(), 2);
        assert_eq!(la[0].rect_mm.x, 0.0);
        let w = la[0].rect_mm.w;
        assert!((w - 1920.0 / 96.0 * 25.4).abs() < 1e-9, "96 dpi assumption");
        assert!(
            (la[1].rect_mm.x - w).abs() < 1e-9,
            "second monitor follows the OS arrangement"
        );

        let lb = with_own_screens(
            Some(&update(la.clone())),
            b,
            &[local(0, (2560, 1600), (0, 0))],
        );
        assert_eq!(lb.len(), 3);
        let bs = lb.iter().find(|s| s.device == b).unwrap();
        assert!(
            (bs.rect_mm.x - 2.0 * w).abs() < 1e-9,
            "new device to the right"
        );
    }

    #[test]
    fn user_positions_survive_and_changed_screens_are_rebuilt() {
        let a = DeviceId([1; 32]);
        let mut placed = with_own_screens(None, a, &[local(0, (1920, 1080), (0, 0))]);
        placed[0].rect_mm.x = 123.0;
        placed[0].rect_mm.y = -40.0;
        let same = with_own_screens(
            Some(&update(placed.clone())),
            a,
            &[local(0, (1920, 1080), (0, 0))],
        );
        assert_eq!(same, placed, "nothing changed");
        let resized = with_own_screens(
            Some(&update(placed.clone())),
            a,
            &[local(0, (3840, 2160), (0, 0))],
        );
        assert_eq!(resized[0].px_width, 3840);
        assert!(
            (resized[0].rect_mm.x - 123.0).abs() < 1e-9,
            "anchored where the user put it"
        );
        let gone = with_own_screens(Some(&update(placed)), a, &[]);
        assert!(gone.is_empty());
    }

    #[test]
    fn edid_size_wins_over_dpi_guess() {
        let mut s = local(0, (3840, 2160), (0, 0));
        s.size_mm = Some((597.0, 336.0));
        assert_eq!(s.size_mm(), (597.0, 336.0));
        s.size_mm = Some((0.0, 0.0));
        s.scale_permille = 2000;
        let (w, _) = s.size_mm();
        assert!((w - 3840.0 / 192.0 * 25.4).abs() < 1e-9);
    }
}
