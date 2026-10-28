<script lang="ts">
  import { daemon } from '../lib/daemon.svelte';
  import { settings } from '../lib/settings.svelte';

  const t = $derived(settings.t);
  let name = $state('');
  let autoJoin = $state(true);
  const mode = $derived(daemon.status?.state ?? 'Connected');
  const label = $derived(
    mode === 'Paused' ? t.common.paused : mode === 'Left' ? t.common.left : t.common.online,
  );

  async function saveProfile() {
    if (await daemon.run({ SaveProfile: { name: name.trim(), auto_join: autoJoin } })) name = '';
  }
</script>

<h1>{t.network.title}</h1>
<div class="card">
  <div class="row">
    <strong>{t.network.state}:</strong> <span data-testid="state">{label}</span>
    {#if daemon.status?.network_profile}<span class="muted">· {daemon.status.network_profile}</span>{/if}
  </div>
  <div class="row" style="margin-top:8px">
    {#if mode === 'Connected'}
      <button onclick={() => daemon.run('Pause')}>{t.network.pause}</button>
      <button onclick={() => daemon.run('Leave')}>{t.network.leave}</button>
    {:else if mode === 'Paused'}
      <button class="primary" onclick={() => daemon.run('Resume')}>{t.network.resume}</button>
      <button onclick={() => daemon.run('Leave')}>{t.network.leave}</button>
    {:else}
      <button class="primary" onclick={() => daemon.run('Rejoin')}>{t.network.rejoin}</button>
    {/if}
  </div>
  <p class="muted">{t.network.pauseHint} {t.network.leaveHint}</p>
</div>

<h2>{t.network.profiles}</h2>
{#each daemon.profiles as p (p.name)}
  <div class="card row">
    <strong>{p.name}</strong>
    {#if p.current}<span class="muted">({t.network.current})</span>{/if}
    <span class="muted">{p.auto_join ? t.network.autoJoin : ''}</span>
    <span style="flex:1"></span>
    <button onclick={() => daemon.run({ ForgetProfile: { name: p.name } })}>{t.network.forget}</button>
  </div>
{:else}
  <p class="muted">{t.network.profilesNone}</p>
{/each}
<div class="card row">
  <input placeholder={t.network.profileName} bind:value={name} aria-label={t.network.profileName} />
  <label><input type="checkbox" bind:checked={autoJoin} /> {t.network.autoJoin}</label>
  <button class="primary" disabled={!name.trim()} onclick={saveProfile}>{t.common.save}</button>
</div>
