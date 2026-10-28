<script lang="ts">
  import { base32, sameId } from '../lib/base32';
  import { daemon } from '../lib/daemon.svelte';
  import { bounds, devices, deviceBounds, moveDevice, snap } from '../lib/layout';
  import { settings } from '../lib/settings.svelte';
  import type { DeviceId, ScreenPlacement } from '../lib/types';

  const t = $derived(settings.t);
  let draft = $state<ScreenPlacement[]>([]);
  let dirty = $state(false);
  let saved = $state(false);
  let seen = -1;
  let svg: SVGSVGElement | undefined = $state();
  let drag: { device: DeviceId; start: { x: number; y: number }; base: ScreenPlacement[] } | null = null;

  $effect(() => {
    const l = daemon.status?.layout;
    if (l && l.version.counter !== seen && !dirty && !drag) {
      seen = l.version.counter;
      draft = l.screens.map((s) => ({ ...s, rect_mm: { ...s.rect_mm } }));
    }
  });

  const box = $derived.by(() => {
    const b = bounds(draft.map((s) => s.rect_mm));
    const m = Math.max(b.w, b.h) * 0.08 + 20;
    return { x: b.x - m, y: b.y - m, w: b.w + 2 * m, h: b.h + 2 * m };
  });

  function point(e: PointerEvent) {
    const ctm = svg?.getScreenCTM();
    if (!svg || !ctm) return { x: 0, y: 0 };
    const p = new DOMPoint(e.clientX, e.clientY).matrixTransform(ctm.inverse());
    return { x: p.x, y: p.y };
  }

  function down(e: PointerEvent, device: DeviceId) {
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    drag = { device, start: point(e), base: draft };
  }

  function move(e: PointerEvent) {
    if (!drag) return;
    const p = point(e);
    draft = moveDevice(drag.base, drag.device, p.x - drag.start.x, p.y - drag.start.y);
  }

  function up() {
    if (!drag) return;
    draft = snap(draft, drag.device);
    drag = null;
    dirty = true;
    saved = false;
  }

  async function save() {
    if (await daemon.run({ SetLayout: { screens: draft } })) {
      dirty = false;
      saved = true;
    }
  }

  const me = $derived(daemon.status?.device_id);
</script>

<h1>{t.layout.title}</h1>
<p class="muted">{t.layout.hint}</p>
{#if draft.length === 0}
  <p class="muted">{t.layout.empty}</p>
{:else}
  <svg
    bind:this={svg}
    viewBox="{box.x} {box.y} {box.w} {box.h}"
    class="editor"
    role="application"
    onpointermove={move}
    onpointerup={up}
  >
    {#each devices(draft) as d (base32(d))}
      {@const b = deviceBounds(draft, d)}
      <g
        class="device"
        class:me={me && sameId(d, me)}
        data-device={base32(d)}
        role="button"
        tabindex="0"
        onpointerdown={(e) => down(e, d)}
      >
        {#each draft.filter((s) => sameId(s.device, d)) as s (s.screen)}
          <rect x={s.rect_mm.x} y={s.rect_mm.y} width={s.rect_mm.w} height={s.rect_mm.h} rx="4" />
          <text x={s.rect_mm.x + s.rect_mm.w / 2} y={s.rect_mm.y + s.rect_mm.h / 2} font-size={Math.max(box.w, box.h) / 40}>
            {s.px_width}×{s.px_height}
          </text>
        {/each}
        <text x={b.x + 6} y={b.y + Math.max(box.w, box.h) / 30} class="name" font-size={Math.max(box.w, box.h) / 34}>
          {daemon.nameOf(d)}
        </text>
      </g>
    {/each}
  </svg>
  <div class="row" style="margin-top:8px">
    <button class="primary" disabled={!dirty} onclick={save} data-testid="save-layout">{t.common.save}</button>
    {#if saved}<span class="muted">{t.layout.saved}</span>{/if}
  </div>
{/if}

<style>
  .editor {
    width: 100%;
    height: 420px;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 8px;
    touch-action: none;
    user-select: none;
  }
  .device rect {
    fill: var(--screen);
    stroke: var(--accent);
    stroke-width: 1.5;
    cursor: grab;
  }
  .device.me rect {
    fill: var(--screen-me);
  }
  .device text {
    fill: var(--text);
    text-anchor: middle;
    dominant-baseline: middle;
    pointer-events: none;
  }
  .device text.name {
    text-anchor: start;
    font-weight: 600;
  }
</style>
