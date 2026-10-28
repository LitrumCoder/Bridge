<script lang="ts">
  import { onDestroy, onMount } from 'svelte';

  import { daemon } from './lib/daemon.svelte';
  import { settings } from './lib/settings.svelte';
  import Devices from './sections/Devices.svelte';
  import Files from './sections/Files.svelte';
  import Layout from './sections/Layout.svelte';
  import Midi from './sections/Midi.svelte';
  import Network from './sections/Network.svelte';
  import Settings from './sections/Settings.svelte';

  type Section = 'devices' | 'layout' | 'files' | 'midi' | 'network' | 'settings';
  const sections: Section[] = ['devices', 'layout', 'files', 'midi', 'network', 'settings'];
  let current = $state<Section>('devices');
  const t = $derived(settings.t);

  onMount(() => daemon.start());
  onDestroy(() => daemon.stop());

  const cursorText = $derived.by(() => {
    const c = daemon.cursor;
    if (c.kind === 'on') return `${t.cursor.on} ${daemon.nameOf(c.device)}`;
    if (c.kind === 'locked') return t.cursor.locked;
    return t.cursor.here;
  });
</script>

<div class="shell">
  <nav>
    <div class="brand">Bridge</div>
    {#each sections as s (s)}
      <button class:active={current === s} onclick={() => (current = s)} data-section={s}>{t.nav[s]}</button>
    {/each}
    <div class="status muted" data-testid="cursor">{cursorText}</div>
  </nav>
  <main>
    {#if daemon.down}
      <div class="card error" role="alert">{t.common.daemonDown}</div>
    {/if}
    {#if daemon.error && !daemon.down}
      <div class="card error" role="alert">{daemon.error}</div>
    {/if}
    {#if current === 'devices'}
      <Devices />
    {:else if current === 'layout'}
      <Layout />
    {:else if current === 'files'}
      <Files />
    {:else if current === 'midi'}
      <Midi />
    {:else if current === 'network'}
      <Network />
    {:else}
      <Settings />
    {/if}
  </main>
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 190px 1fr;
    min-height: 100vh;
  }
  nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 12px 8px;
    border-right: 1px solid var(--line);
    background: var(--panel);
  }
  nav button {
    text-align: left;
    border: none;
    background: transparent;
  }
  nav button.active {
    background: var(--bg);
    font-weight: 600;
  }
  .brand {
    font-weight: 700;
    font-size: 16px;
    padding: 4px 12px 12px;
  }
  .status {
    margin-top: auto;
    padding: 8px 12px;
    font-size: 12px;
  }
  main {
    padding: 20px 24px;
    max-width: 920px;
  }
</style>
