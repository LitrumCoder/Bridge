<script lang="ts">
  import { base32, fingerprint, sameId } from '../lib/base32';
  import { daemon } from '../lib/daemon.svelte';
  import { expect, qrSvg } from '../lib/ipc';
  import { settings } from '../lib/settings.svelte';
  import { type DeviceEntry, PERMISSIONS } from '../lib/types';

  const t = $derived(settings.t);
  let invite = $state<{ code: string; qr: string; expires: number } | null>(null);
  let target = $state('');
  let code = $state('');
  let paired = $state<string | null>(null);

  async function showCode() {
    const r = await daemon.run('OpenPairing');
    if (!r) return;
    const i = expect(r, 'PairingInvite');
    invite = { code: i.code, qr: await qrSvg(i.qr_uri).catch(() => ''), expires: i.expires_secs };
  }

  async function pair() {
    const addr = /^[\d.]+:\d+$|^\[.*\]:\d+$/.test(target.trim());
    const r = await daemon.run({
      Pair: { target: addr ? { Address: target.trim() } : { Nearby: target.trim() }, code: code.replace(/\s/g, '') },
    });
    if (r) {
      paired = fingerprint(expect(r, 'Paired').device);
      code = '';
    }
  }

  function online(d: DeviceEntry) {
    const peer = daemon.status?.peers.find((p) => sameId(p.id, d.id));
    if (!peer) return t.common.offline;
    if (peer.state === 'Paused') return t.common.paused;
    return peer.rtt_us != null ? `${t.common.online}, ${t.devices.rtt} ${(peer.rtt_us / 1000).toFixed(1)} ms` : t.common.online;
  }

  function toggle(d: DeviceEntry, perm: number) {
    const granted = d.granted.includes(perm) ? d.granted.filter((g) => g !== perm) : [...d.granted, perm];
    void daemon.run({ SetPermissions: { device: base32(d.id), granted } });
  }

  const perms = [
    ['input', PERMISSIONS.input],
    ['files', PERMISSIONS.files],
    ['clipboard', PERMISSIONS.clipboard],
    ['midi', PERMISSIONS.midi],
  ] as const;
</script>

<h1>{t.devices.title}</h1>
{#if daemon.devices.length === 0}
  <p class="muted">{t.devices.none}</p>
{/if}
{#each daemon.devices as d (base32(d.id))}
  <div class="card" data-testid="device">
    <div class="row">
      <strong>{d.name ?? fingerprint(d.id)}</strong>
      <span class="muted">{fingerprint(d.id)} · {online(d)}</span>
      <span style="flex:1"></span>
      <button onclick={() => daemon.run({ Unpair: { device: base32(d.id) } })}>{t.devices.unpair}</button>
    </div>
    <div class="row muted" style="margin-top:8px">
      {t.devices.permissions}:
      {#each perms as [key, id] (key)}
        <label><input type="checkbox" checked={d.granted.includes(id)} onchange={() => toggle(d, id)} /> {t.devices[key]}</label>
      {/each}
    </div>
  </div>
{/each}

<h2>{t.devices.nearby}</h2>
{#each daemon.nearby.filter((n) => !n.paired) as n (base32(n.id))}
  <div class="card row">
    <strong>{n.name ?? fingerprint(n.id)}</strong>
    <span class="muted">{fingerprint(n.id)} · {n.addrs[0] ?? ''}</span>
    <button onclick={() => (target = fingerprint(n.id))}>{t.devices.pairWith}</button>
  </div>
{:else}
  <p class="muted">{t.devices.nearbyNone}</p>
{/each}

<div class="card">
  <div class="row">
    <input placeholder={t.devices.pairTarget} bind:value={target} aria-label={t.devices.pairTarget} />
    <input placeholder="000 000" bind:value={code} size="8" aria-label={t.devices.pairCode} />
    <button class="primary" disabled={!target || code.replace(/\s/g, '').length !== 6} onclick={pair}>{t.devices.pairWith}</button>
    {#if paired}<span class="muted">{t.devices.pairDone}: {paired}</span>{/if}
  </div>
  <div class="row" style="margin-top:8px">
    <button onclick={showCode} data-testid="show-code">{t.devices.pairShow}</button>
  </div>
  {#if invite}
    <div class="row" style="margin-top:8px">
      {#if invite.qr}<div class="qr">{@html invite.qr}</div>{/if}
      <div>
        <div class="muted">{t.devices.pairCode}</div>
        <div class="code" data-testid="pair-code">{invite.code.slice(0, 3)} {invite.code.slice(3)}</div>
        <div class="muted">{t.devices.pairHint}</div>
      </div>
    </div>
  {/if}
</div>

<style>
  .code {
    font-size: 28px;
    font-weight: 700;
    letter-spacing: 2px;
  }
  .qr :global(svg) {
    width: 140px;
    height: 140px;
    background: white;
  }
</style>
