// The daemon's IPC types as serde_json shows them (crates/ipc/src/lib.rs, status.rs).

export type DeviceId = number[];
export type DeviceState = 'Connected' | 'Paused' | 'Left';

export interface RectMm {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface ScreenPlacement {
  device: DeviceId;
  screen: number;
  px_width: number;
  px_height: number;
  origin_x: number;
  origin_y: number;
  scale_permille: number;
  rect_mm: RectMm;
}

export interface LayoutUpdate {
  version: { counter: number; device: DeviceId };
  screens: ScreenPlacement[];
}

export interface PeerStatus {
  id: DeviceId;
  name: string;
  platform: number;
  app_version: string;
  rtt_us: number | null;
  clock_offset_us: number | null;
  state: DeviceState;
}

export interface NodeStatus {
  device_id: DeviceId;
  name: string;
  listen: string;
  state: DeviceState;
  peers: PeerStatus[];
  layout: LayoutUpdate | null;
  network_profile: string | null;
}

export interface DeviceEntry {
  id: DeviceId;
  name: string | null;
  connected: boolean;
  granted: number[];
  last_seen: number | null;
}

export interface NearbyEntry {
  id: DeviceId;
  name: string | null;
  addrs: string[];
  paired: boolean;
}

export interface TransferEntry {
  id: string;
  device: DeviceId;
  outgoing: boolean;
  items: number;
  bytes: number;
  done_bytes: number;
  state: string;
  names: string[];
}

export interface ProfileEntry {
  name: string;
  networks: number;
  auto_join: boolean;
  current: boolean;
}

export interface MidiStreamEntry {
  device: DeviceId;
  port: number;
  to_instrument: boolean;
  events: number;
  late: number;
  min_transit_us: number;
  buffer_us: number;
}

export type UiEvent = { Entered: { device: DeviceId } } | 'Returned' | { Locked: boolean } | { Shake: { device: DeviceId } };

export type PairTarget = { Nearby: string } | { Address: string } | { Uri: string };

export type Request =
  | 'Status'
  | 'Devices'
  | 'Nearby'
  | 'OpenPairing'
  | 'ClosePairing'
  | 'Shutdown'
  | 'Transfers'
  | 'Pause'
  | 'Resume'
  | 'Leave'
  | 'Rejoin'
  | 'Profiles'
  | 'MidiStreams'
  | { Pair: { target: PairTarget; code: string } }
  | { Unpair: { device: string } }
  | { SetPermissions: { device: string; granted: number[] } }
  | { Send: { device: string; paths: string[] } }
  | { CancelTransfer: { id: string } }
  | { SaveProfile: { name: string; auto_join: boolean } }
  | { ForgetProfile: { name: string } }
  | { SetLayout: { screens: ScreenPlacement[] } }
  | { Events: { since: number } };

export type Response =
  | 'Ok'
  | { Status: NodeStatus }
  | { Devices: DeviceEntry[] }
  | { Nearby: NearbyEntry[] }
  | { PairingInvite: { code: string; qr_uri: string; expires_secs: number } }
  | { Paired: { device: DeviceId } }
  | { Error: string }
  | { Transfer: { id: string } }
  | { Transfers: TransferEntry[] }
  | { Profiles: ProfileEntry[] }
  | { Midi: MidiStreamEntry[] }
  | { Events: { next: number; events: UiEvent[] } };

/** Permission ids (PermissionId in the protocol crate). */
export const PERMISSIONS = { input: 1, files: 2, clipboard: 3, midi: 4 } as const;
