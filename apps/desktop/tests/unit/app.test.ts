import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import App from '../../src/App.svelte';
import { base32 } from '../../src/lib/base32';
import { daemon } from '../../src/lib/daemon.svelte';
import { LAPTOP, handle, initialState } from '../../src/lib/mock';
import { settings } from '../../src/lib/settings.svelte';
import type { Request } from '../../src/lib/types';

let requests: Request[] = [];

beforeEach(() => {
  const s = initialState();
  requests = [];
  settings.setLang('en');
  mockIPC((cmd, args) => {
    if (cmd === 'ipc') {
      const r = (args as { request: Request }).request;
      requests.push(r);
      return handle(s, r);
    }
    if (cmd === 'qr_svg') return '<svg data-testid="qr"></svg>';
    return null;
  });
});

afterEach(() => {
  daemon.stop();
  cleanup();
  clearMocks();
});

describe('App', () => {
  it('lists paired devices and the cursor location from daemon events', async () => {
    render(App);
    expect(await screen.findByText('laptop')).toBeTruthy();
    expect(screen.getByText('phone')).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId('cursor').textContent).toContain('laptop'));
  });

  it('sends SetPermissions with the toggled permission', async () => {
    render(App);
    await screen.findByText('laptop');
    const boxes = screen.getAllByLabelText('MIDI');
    await fireEvent.click(boxes[0]!);
    await waitFor(() =>
      expect(requests).toContainEqual({ SetPermissions: { device: base32(LAPTOP), granted: [1, 2, 3, 4] } }),
    );
  });

  it('shows a pairing code and QR', async () => {
    render(App);
    await fireEvent.click(await screen.findByTestId('show-code'));
    expect((await screen.findByTestId('pair-code')).textContent).toBe('483 921');
    expect(screen.getByTestId('qr')).toBeTruthy();
  });

  it('pauses and resumes from the network section', async () => {
    render(App);
    await fireEvent.click(await screen.findByText('Network', { selector: 'button' }));
    await fireEvent.click(await screen.findByText('Pause'));
    await waitFor(() => expect(screen.getByTestId('state').textContent).toBe(settings.t.common.paused));
    await fireEvent.click(screen.getByText('Resume'));
    await waitFor(() => expect(requests).toContain('Resume'));
  });

  it('reports a dead daemon instead of crashing', async () => {
    clearMocks();
    mockIPC(() => {
      throw new Error('daemon is not running');
    });
    render(App);
    expect(await screen.findByRole('alert')).toBeTruthy();
  });
});
