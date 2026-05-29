<script>
  export let source;
  export let busy;
  export let saveConfig;
</script>

<div class="source">
  <textarea bind:value={source} spellcheck="false"></textarea>
  <button disabled={busy} on:click={saveConfig}>Save config</button>
</div>
