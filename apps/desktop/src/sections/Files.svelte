<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';

  import { base32 } from '../lib/base32';
  import { daemon } from '../lib/daemon.svelte';
  import { settings } from '../lib/settings.svelte';

  const t = $derived(settings.t);
  let to = $state('');

  const connected = $derived(daemon.devices.filter((d) => d.connected));

  async function send() {
    const picked = await open({ multiple: true, directory: false });
    const paths = picked == null ? [] : Array.isArray(picked) ? picked : [picked];
    if (paths.length > 0 && to) {
      await daemon.run({ Send: { device: to, paths } });
    }
  }

  function size(n: number) {
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i++;
    }
    return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
  }
</script>

<h1>{t.files.title}</h1>
<div class="row card">
  <label>{t.files.to}
    <select bind:value={to} aria-label={t.files.to}>
      {#each connected as d (base32(d.id))}
        <option value={base32(d.id)}>{d.name ?? base32(d.id).slice(0, 8)}</option>
      {/each}
    </select>
  </label>
  <button class="primary" disabled={!to} onclick={send}>{t.files.send}</button>
</div>

{#each daemon.transfers as tr (tr.id)}
  <div class="card" data-testid="transfer">
    <div class="row">
      <strong>{tr.names.join(', ')}</strong>
      <span class="muted">{tr.outgoing ? t.files.outgoing : t.files.incoming} · {daemon.nameOf(tr.device)} · {tr.items} · {size(tr.bytes)}</span>
      <span style="flex:1"></span>
      <span>{tr.state}</span>
      {#if tr.state === 'running'}
        <button onclick={() => daemon.run({ CancelTransfer: { id: tr.id } })}>{t.files.cancel}</button>
      {/if}
    </div>
    <progress max={Math.max(tr.bytes, 1)} value={tr.done_bytes}></progress>
  </div>
{:else}
  <p class="muted">{t.files.none}</p>
{/each}

<style>
  progress {
    width: 100%;
    margin-top: 6px;
  }
</style>
