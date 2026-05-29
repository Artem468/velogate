<script>
  export let state;
  export let busy;
  export let mode;
  export let lastSavedAt;
  export let visibleEndpointIndexes = [];
  export let addEndpoint;
  export let addDraftNode;
  export let relayoutGraph;
  export let setEndpointVisible;
  export let showAllEndpoints;
  export let hideAllEndpoints;

  $: endpoints = state?.model.parsed?.file.endpoints ?? [];

  function visible(index) {
    return visibleEndpointIndexes.includes(index);
  }
</script>

<header>
  <div>
    <strong>{state?.config_path ?? 'loading'}</strong>
    <span class={state?.runtime_running ? 'ok' : 'off'}>{state?.runtime_running ? 'running' : 'stopped'}</span>
    <small>{lastSavedAt ? `saved ${lastSavedAt}` : 'graph updates save to .gate via websocket'}</small>
  </div>

  <details class="endpoint-filter">
    <summary>{visibleEndpointIndexes.length}/{endpoints.length} endpoints</summary>
    <div class="endpoint-filter-menu">
      <div class="endpoint-filter-actions">
        <button type="button" on:click={showAllEndpoints}>All</button>
        <button type="button" on:click={hideAllEndpoints}>None</button>
      </div>
      {#key visibleEndpointIndexes.join(',')}
        {#each endpoints as endpoint, index}
          <label class="inline-check">
            <input type="checkbox" checked={visible(index)} on:change={(event) => setEndpointVisible(index, event.currentTarget.checked)} />
            <span>{endpoint.method} {endpoint.path}</span>
          </label>
        {/each}
      {/key}
    </div>
  </details>

  <div class="actions">
    <button disabled={busy || mode !== 'graph'} on:click={addEndpoint}>+ Endpoint</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('fetch')}>+ Fetch</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('let')}>+ Let</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('pipe')}>+ Transform</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('db')}>+ DB</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('grpc')}>+ gRPC</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('command')}>+ Command</button>
    <button disabled={busy || mode !== 'graph'} on:click={() => addDraftNode('step')}>+ Code</button>
    <button disabled={busy || mode !== 'graph'} on:click={relayoutGraph}>Fit</button>
  </div>
</header>
