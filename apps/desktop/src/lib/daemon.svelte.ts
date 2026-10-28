import { sameId } from './base32';
import { call, expect } from './ipc';
import type {
  DeviceEntry,
  DeviceId,
  MidiStreamEntry,
  NearbyEntry,
  NodeStatus,
  ProfileEntry,
  Request,
  TransferEntry,
  UiEvent,
} from './types';

export type Cursor = { kind: 'here' } | { kind: 'on'; device: DeviceId } | { kind: 'locked' };

/** What the UI shows, refreshed from the daemon every second. Nothing is decided here. */
class Daemon {
  status = $state<NodeStatus | null>(null);
  devices = $state<DeviceEntry[]>([]);
  nearby = $state<NearbyEntry[]>([]);
  transfers = $state<TransferEntry[]>([]);
  profiles = $state<ProfileEntry[]>([]);
  midi = $state<MidiStreamEntry[]>([]);
  cursor = $state<Cursor>({ kind: 'here' });
  down = $state(false);
  error = $state<string | null>(null);
  private since = 0;
  private timer: ReturnType<typeof setInterval> | null = null;

  async refresh() {
    try {
      const [status, devices, nearby, transfers, profiles, midi, events] = await Promise.all([
        call('Status'),
        call('Devices'),
        call('Nearby'),
        call('Transfers').catch(() => ({ Transfers: [] })),
        call('Profiles'),
        call('MidiStreams'),
        call({ Events: { since: this.since } }),
      ]);
      this.status = expect(status, 'Status');
      this.devices = expect(devices, 'Devices');
      this.nearby = expect(nearby, 'Nearby');
      this.transfers = expect(transfers, 'Transfers');
      this.profiles = expect(profiles, 'Profiles');
      this.midi = expect(midi, 'Midi');
      const ev = expect(events, 'Events');
      this.since = ev.next;
      for (const e of ev.events) this.apply(e);
      this.down = false;
    } catch (e) {
      this.down = true;
      this.error = String(e);
    }
  }

  apply(e: UiEvent) {
    if (e === 'Returned') this.cursor = { kind: 'here' };
    else if ('Entered' in e) this.cursor = { kind: 'on', device: e.Entered.device };
    else if ('Locked' in e && e.Locked) this.cursor = { kind: 'locked' };
  }

  /** Runs a command, reports a daemon error, and refreshes at once. */
  async run(request: Request) {
    try {
      const r = await call(request);
      this.error = null;
      await this.refresh();
      return r;
    } catch (e) {
      this.error = String(e instanceof Error ? e.message : e);
      return null;
    }
  }

  nameOf(id: DeviceId): string {
    return (
      this.status?.peers.find((p) => sameId(p.id, id))?.name ??
      this.devices.find((d) => sameId(d.id, id))?.name ??
      (this.status && sameId(this.status.device_id, id) ? this.status.name : '')
    );
  }

  start() {
    void this.refresh();
    this.timer ??= setInterval(() => void this.refresh(), 1000);
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }
}

export const daemon = new Daemon();
