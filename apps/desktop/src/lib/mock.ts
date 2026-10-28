import { mockIPC, mockWindows } from '@tauri-apps/api/mocks';

import type { DeviceEntry, NodeStatus, Request, Response, ScreenPlacement, TransferEntry } from './types';

// A fake daemon for `npm run build:mock` and the Playwright tests: it keeps a little state
// and records every request in `window.__bridgeRequests`.

const id = (n: number) => Array.from({ length: 32 }, (_, i) => (i === 0 ? n : i));
export const ME = id(1);
export const LAPTOP = id(2);
export const PHONE = id(3);

function screen(device: number[], x: number, w: number, h: number): ScreenPlacement {
  return {
    device,
    screen: 0,
    px_width: Math.round(w * 4),
    px_height: Math.round(h * 4),
    origin_x: 0,
    origin_y: 0,
    scale_permille: 1000,
    rect_mm: { x, y: 0, w, h },
  };
}

interface MockState {
  status: NodeStatus;
  devices: DeviceEntry[];
  transfers: TransferEntry[];
  profiles: { name: string; networks: number; auto_join: boolean; current: boolean }[];
  events: Response;
}

export function initialState(): MockState {
  return {
    status: {
      device_id: ME,
      name: 'desk',
      listen: '0.0.0.0:47800',
      state: 'Connected',
      peers: [{ id: LAPTOP, name: 'laptop', platform: 1, app_version: '0.9.0', rtt_us: 900, clock_offset_us: 10, state: 'Connected' }],
      layout: {
        version: { counter: 3, device: ME },
        screens: [screen(ME, 0, 600, 340), screen(LAPTOP, 600, 300, 190)],
      },
      network_profile: null,
    },
    devices: [
      { id: LAPTOP, name: 'laptop', connected: true, granted: [1, 2, 3], last_seen: null },
      { id: PHONE, name: 'phone', connected: false, granted: [2], last_seen: 1_700_000_000 },
    ],
    transfers: [],
    profiles: [],
    events: { Events: { next: 1, events: [{ Entered: { device: LAPTOP } }] } },
  };
}

function key(r: Request): string {
  return typeof r === 'string' ? r : (Object.keys(r)[0] ?? '');
}

export function handle(s: MockState, r: Request): Response {
  switch (key(r)) {
    case 'Status':
      return { Status: s.status };
    case 'Devices':
      return { Devices: s.devices };
    case 'Nearby':
      return { Nearby: [{ id: id(4), name: 'tv', addrs: ['192.168.1.9:47800'], paired: false }] };
    case 'Transfers':
      return { Transfers: s.transfers };
    case 'Profiles':
      return { Profiles: s.profiles };
    case 'MidiStreams':
      return { Midi: [{ device: LAPTOP, port: 0, to_instrument: false, events: 42, late: 0, min_transit_us: 700, buffer_us: 3000 }] };
    case 'Events': {
      const e = s.events;
      s.events = { Events: { next: 1, events: [] } };
      return e;
    }
    case 'OpenPairing':
      return { PairingInvite: { code: '483921', qr_uri: 'bridge://pair?c=483921', expires_secs: 120 } };
    case 'Pause':
    case 'Resume':
    case 'Leave':
    case 'Rejoin':
      s.status = { ...s.status, state: key(r) === 'Pause' ? 'Paused' : key(r) === 'Leave' ? 'Left' : 'Connected' };
      return 'Ok';
    case 'SetLayout': {
      const screens = (r as { SetLayout: { screens: ScreenPlacement[] } }).SetLayout.screens;
      const counter = (s.status.layout?.version.counter ?? 0) + 1;
      s.status = { ...s.status, layout: { version: { counter, device: ME }, screens } };
      return 'Ok';
    }
    case 'SetPermissions': {
      const p = (r as { SetPermissions: { device: string; granted: number[] } }).SetPermissions;
      s.devices = s.devices.map((d, i) => (i === 0 ? { ...d, granted: p.granted } : d));
      return 'Ok';
    }
    case 'SaveProfile': {
      const p = (r as { SaveProfile: { name: string; auto_join: boolean } }).SaveProfile;
      s.profiles = [...s.profiles, { name: p.name, networks: 1, auto_join: p.auto_join, current: true }];
      return 'Ok';
    }
    case 'Send': {
      const p = (r as { Send: { device: string; paths: string[] } }).Send;
      s.transfers = [
        ...s.transfers,
        { id: `t${s.transfers.length + 1}`, device: LAPTOP, outgoing: true, items: p.paths.length, bytes: 1 << 20, done_bytes: 1 << 19, state: 'running', names: p.paths.map((x) => x.split(/[\\/]/).pop() ?? x) },
      ];
      return { Transfer: { id: `t${s.transfers.length}` } };
    }
    case 'CancelTransfer': {
      const tid = (r as { CancelTransfer: { id: string } }).CancelTransfer.id;
      s.transfers = s.transfers.map((t) => (t.id === tid ? { ...t, state: 'cancelled' } : t));
      return 'Ok';
    }
    default:
      return 'Ok';
  }
}

declare global {
  interface Window {
    __bridgeRequests: Request[];
  }
}

export function install() {
  const s = initialState();
  window.__bridgeRequests = [];
  mockWindows('main');
  mockIPC((cmd, args) => {
    if (cmd === 'ipc') {
      const request = (args as { request: Request }).request;
      window.__bridgeRequests.push(request);
      return handle(s, request);
    }
    if (cmd === 'qr_svg') return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>';
    if (cmd === 'plugin:dialog|open') return ['C:\\Users\\me\\photo.jpg'];
    return null;
  });
}
