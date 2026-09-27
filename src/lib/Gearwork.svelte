<script lang="ts">
  let { class: klass = "" }: { class?: string } = $props();

  function gear(cx: number, cy: number, radius: number, teeth: number) {
    return Array.from({ length: teeth * 4 }, (_, i) => {
      const angle = (i / (teeth * 4)) * Math.PI * 2;
      const r = radius + (i % 4 === 1 || i % 4 === 2 ? 5 : 0);
      return `${i ? "L" : "M"}${cx + Math.cos(angle) * r},${cy + Math.sin(angle) * r}`;
    }).join(" ") + " Z";
  }
</script>

<svg class="gearwork {klass}" viewBox="0 0 340 200" fill="none" aria-hidden="true">
  <g stroke="currentColor" stroke-width=".8">
    <path d="M12 146H328M220 12V188M31 29L295 181M35 179L298 27" opacity=".3" />
    <circle cx="220" cy="104" r="87" stroke-dasharray="2 5" />
    <circle cx="220" cy="104" r="72" />
    <path d={gear(220, 104, 62, 24)} />
    <circle cx="220" cy="104" r="49" />
    <circle cx="220" cy="104" r="38" />
    <circle cx="220" cy="104" r="10" />
    {#each [0, 60, 120, 180, 240, 300] as angle}
      <path d="M233 99L253 89L258 98L235 108Z" transform="rotate({angle} 220 104)" />
    {/each}
    <path d={gear(110, 143, 44, 17)} />
    <circle cx="110" cy="143" r="32" />
    <circle cx="110" cy="143" r="7" />
    {#each [0, 90, 180, 270] as angle}
      <path d="M121 138L139 135L140 144L122 148Z" transform="rotate({angle} 110 143)" />
    {/each}
    <path d={gear(129, 55, 30, 12)} />
    <circle cx="129" cy="55" r="21" />
    <circle cx="129" cy="55" r="6" />
    <path d="M129 34V49M129 61V76M108 55H123M135 55H150M30 143H55M110 196V184M220 5V18M306 104H330" />
    <path d="M24 39V25H38M308 174H322V160" />
    <circle cx="31" cy="146" r="3" />
  </g>
  <g stroke="var(--color-accent)" stroke-width="1.4">
    <path d="M217 104H223M220 101V107M31 33H52M31 36H43" />
    <path d="M268 32A86 86 0 0 1 305 85" />
  </g>
</svg>
