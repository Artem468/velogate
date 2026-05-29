<script>
  export let mode;
  export let busy;
  export let autoSaveGraph;
  export let setMode;
  export let saveGraph;
  export let toggleAutoSave;
  export let control;
</script>

<aside class="rail">
  <div class="brand">VeloGate</div>
  <div class="rail-section">
    <button class:active={mode === 'graph'} on:click={() => setMode('graph')} title="Graph">G</button>
    <button class:active={mode === 'source'} on:click={() => setMode('source')} title="Source">&#123;&#125;</button>
  </div>
  <div class="rail-section">
    <button disabled={busy || mode !== 'graph'} on:click={() => saveGraph(false)} title="Save graph">Save</button>
    <button class:active={autoSaveGraph} disabled={busy || mode !== 'graph'} on:click={toggleAutoSave} title="Toggle autosave">Auto</button>
  </div>
  <div class="rail-section">
    <button disabled={busy} on:click={() => control('start')} title="Start runtime">Run</button>
    <button disabled={busy} on:click={() => control('stop')} title="Stop runtime">Stop</button>
    <button disabled={busy} on:click={() => control('restart')} title="Restart runtime">Rst</button>
  </div>
</aside>
