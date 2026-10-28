<script lang="ts">
  import { base32 } from '../lib/base32';
  import { daemon } from '../lib/daemon.svelte';
  import { settings } from '../lib/settings.svelte';

  const t = $derived(settings.t);
</script>

<h1>{t.midi.title}</h1>
{#if daemon.midi.length === 0}
  <p class="muted">{t.midi.none}</p>
{:else}
  <table>
    <thead>
      <tr><th>{t.midi.port}</th><th></th><th>{t.midi.events}</th><th>{t.midi.late}</th><th>{t.midi.latency}</th></tr>
    </thead>
    <tbody>
      {#each daemon.midi as m (base32(m.device) + m.port + m.to_instrument)}
        <tr>
          <td>{daemon.nameOf(m.device)} #{m.port}</td>
          <td class="muted">{m.to_instrument ? t.midi.toInstrument : t.midi.fromInstrument}</td>
          <td>{m.events}</td>
          <td>{m.late}</td>
          <td>{((m.min_transit_us + m.buffer_us) / 1000).toFixed(1)} ms</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}
