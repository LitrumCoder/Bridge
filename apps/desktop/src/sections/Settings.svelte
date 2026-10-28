<script lang="ts">
  import { fingerprint } from '../lib/base32';
  import { daemon } from '../lib/daemon.svelte';
  import { type Lang, type Theme, settings } from '../lib/settings.svelte';

  const t = $derived(settings.t);
  const themes: Theme[] = ['system', 'light', 'dark'];
</script>

<h1>{t.settings.title}</h1>
<div class="card">
  <div class="row">
    <strong>{t.settings.theme}</strong>
    {#each themes as th (th)}
      <label><input type="radio" name="theme" checked={settings.theme === th} onchange={() => settings.setTheme(th)} /> {t.settings[th]}</label>
    {/each}
  </div>
  <div class="row" style="margin-top:8px">
    <strong>{t.settings.language}</strong>
    <select
      value={settings.lang}
      onchange={(e) => settings.setLang((e.currentTarget as HTMLSelectElement).value as Lang)}
      aria-label={t.settings.language}
    >
      <option value="ru">Русский</option>
      <option value="en">English</option>
    </select>
  </div>
</div>
{#if daemon.status}
  <div class="card">
    <strong>{t.settings.device}:</strong>
    {daemon.status.name} · <span class="muted">{fingerprint(daemon.status.device_id)} · {daemon.status.listen}</span>
  </div>
{/if}
