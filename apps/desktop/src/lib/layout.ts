import { sameId } from './base32';
import type { DeviceId, RectMm, ScreenPlacement } from './types';

/** Edges closer than this (mm) snap together, like `SNAP_MM` in the core (2 mm) but generous for a mouse. */
export const SNAP_MM = 12;

export function bounds(rects: RectMm[]): RectMm {
  if (rects.length === 0) return { x: 0, y: 0, w: 1, h: 1 };
  const x = Math.min(...rects.map((r) => r.x));
  const y = Math.min(...rects.map((r) => r.y));
  const r = Math.max(...rects.map((r) => r.x + r.w));
  const b = Math.max(...rects.map((r) => r.y + r.h));
  return { x, y, w: r - x, h: b - y };
}

export function devices(screens: ScreenPlacement[]): DeviceId[] {
  const out: DeviceId[] = [];
  for (const s of screens) {
    if (!out.some((d) => sameId(d, s.device))) out.push(s.device);
  }
  return out;
}

export function deviceBounds(screens: ScreenPlacement[], device: DeviceId): RectMm {
  return bounds(screens.filter((s) => sameId(s.device, device)).map((s) => s.rect_mm));
}

/** Moves every screen of `device` by (dx, dy) millimetres. */
export function moveDevice(screens: ScreenPlacement[], device: DeviceId, dx: number, dy: number): ScreenPlacement[] {
  return screens.map((s) =>
    sameId(s.device, device) ? { ...s, rect_mm: { ...s.rect_mm, x: s.rect_mm.x + dx, y: s.rect_mm.y + dy } } : s,
  );
}

function nearest(own: [number, number], others: [number, number][], limit: number): number {
  let best = 0;
  let bestDist = limit + 1;
  for (const [a, b] of others) {
    for (const delta of [a - own[1], b - own[0], a - own[0], b - own[1]]) {
      if (Math.abs(delta) < bestDist) {
        best = delta;
        bestDist = Math.abs(delta);
      }
    }
  }
  return bestDist <= limit ? best : 0;
}

/** Shifts `device` so its edges meet the nearest edges of other devices within `limit` mm. */
export function snap(screens: ScreenPlacement[], device: DeviceId, limit = SNAP_MM): ScreenPlacement[] {
  const me = deviceBounds(screens, device);
  const others = devices(screens)
    .filter((d) => !sameId(d, device))
    .map((d) => deviceBounds(screens, d));
  const dx = nearest([me.x, me.x + me.w], others.map((o) => [o.x, o.x + o.w]), limit);
  const dy = nearest([me.y, me.y + me.h], others.map((o) => [o.y, o.y + o.h]), limit);
  return dx === 0 && dy === 0 ? screens : moveDevice(screens, device, dx, dy);
}
