import { describe, expect, it } from 'vitest';

import { base32, fingerprint, sameId } from '../../src/lib/base32';
import { en } from '../../src/lib/i18n/en';
import { ru } from '../../src/lib/i18n/ru';
import { bounds, deviceBounds, devices, moveDevice, snap } from '../../src/lib/layout';
import type { ScreenPlacement } from '../../src/lib/types';

const bytes = (s: string) => [...s].map((c) => c.charCodeAt(0));

describe('base32', () => {
  it('matches RFC 4648 vectors in lowercase without padding', () => {
    expect(base32([])).toBe('');
    expect(base32(bytes('f'))).toBe('my');
    expect(base32(bytes('fo'))).toBe('mzxq');
    expect(base32(bytes('foo'))).toBe('mzxw6');
    expect(base32(bytes('foob'))).toBe('mzxw6yq');
    expect(base32(bytes('fooba'))).toBe('mzxw6ytb');
    expect(base32(bytes('foobar'))).toBe('mzxw6ytboi');
  });

  it('gives 52 characters for a device id and an 8-character fingerprint', () => {
    const id = Array.from({ length: 32 }, (_, i) => i * 7);
    expect(base32(id)).toHaveLength(52);
    expect(fingerprint(id)).toBe(base32(id).slice(0, 8));
    expect(sameId(id, [...id])).toBe(true);
    expect(sameId(id, id.slice(1))).toBe(false);
  });
});

function keys(o: object, prefix = ''): string[] {
  return Object.entries(o).flatMap(([k, v]) =>
    typeof v === 'object' && v !== null ? keys(v, `${prefix}${k}.`) : [`${prefix}${k}`],
  );
}

describe('i18n', () => {
  it('has the same keys in both languages and no empty strings', () => {
    expect(keys(en).sort()).toEqual(keys(ru).sort());
    for (const dict of [en, ru]) {
      for (const k of keys(dict)) {
        const v = k.split('.').reduce<unknown>((o, p) => (o as Record<string, unknown>)[p], dict);
        expect(v, k).not.toBe('');
      }
    }
  });
});

const A = [1, 0];
const B = [2, 0];
function scr(device: number[], screen: number, x: number, y: number, w: number, h: number): ScreenPlacement {
  return { device, screen, px_width: 1920, px_height: 1080, origin_x: 0, origin_y: 0, scale_permille: 1000, rect_mm: { x, y, w, h } };
}

describe('layout', () => {
  const layout = [scr(A, 0, 0, 0, 500, 300), scr(A, 1, 500, 0, 500, 300), scr(B, 0, 1100, 50, 300, 200)];

  it('groups screens by device and computes bounds', () => {
    expect(devices(layout)).toEqual([A, B]);
    expect(deviceBounds(layout, A)).toEqual({ x: 0, y: 0, w: 1000, h: 300 });
    expect(bounds([])).toEqual({ x: 0, y: 0, w: 1, h: 1 });
  });

  it('moves all screens of one device together', () => {
    const moved = moveDevice(layout, A, 10, -5);
    expect(moved[0]?.rect_mm).toMatchObject({ x: 10, y: -5 });
    expect(moved[1]?.rect_mm).toMatchObject({ x: 510, y: -5 });
    expect(moved[2]).toBe(layout[2]);
  });

  it('snaps a device edge to a neighbour within the limit', () => {
    const near = moveDevice(layout, B, -92, -45);
    const snapped = snap(near, B);
    expect(deviceBounds(snapped, B)).toMatchObject({ x: 1000, y: 0 });
  });

  it('leaves a device alone when nothing is near', () => {
    expect(snap(layout, B)).toBe(layout);
  });
});
