<script>
  export let selected;
  export let selectedEdge;
  export let nodes = [];
  export let methods = [];
  export let endpointMethods = [];
  export let updateConfig;
  export let mutateSelected;
  export let updateSelectedEdge;
  export let deleteSelectedEdge;
  export let nodeLabel;
  export let handleOptions;
</script>

<aside class="panel">
  <h2>{selected?.data.title ?? selectedEdge?.label ?? 'Layer settings'}</h2>
  {#if selected}
    <label>Layer kind
      <select value={selected.data.config.kind} on:change={(event) => updateConfig('kind', event.currentTarget.value)}>
        <option value="gateway">gateway</option>
        <option value="entry">entry</option>
        <option value="let">let</option>
        <option value="fetch">fetch</option>
        <option value="pipe">pipe</option>
        <option value="db">db</option>
        <option value="grpc">grpc</option>
        <option value="command">command</option>
        <option value="response">response</option>
        <option value="step">step</option>
      </select>
    </label>

    {#if selected.data.config.kind === 'gateway'}
      <label>Name<input value={selected.data.config.name} on:input={(event) => updateConfig('name', event.currentTarget.value)} /></label>
      <label>Port<input type="number" value={selected.data.config.port} on:input={(event) => updateConfig('port', Number(event.currentTarget.value))} /></label>
      <label>Host<input value={selected.data.config.host} on:input={(event) => updateConfig('host', event.currentTarget.value)} /></label>
      <label>Env file<input value={selected.data.config.envFile} on:input={(event) => updateConfig('envFile', event.currentTarget.value)} /></label>
      <div class="section-title">Constants</div>
      <label>Constants code<textarea class="codebox compact" placeholder='"api": env.API_URL' value={selected.data.config.constantsCode} on:input={(event) => updateConfig('constantsCode', event.currentTarget.value)}></textarea></label>
      <div class="section-title">Databases</div>
      <label>Database entries<textarea class="codebox compact" placeholder={'db "main" { url: "postgres://..." }'} value={selected.data.config.databasesCode} on:input={(event) => updateConfig('databasesCode', event.currentTarget.value)}></textarea></label>
      <div class="section-title">Protos</div>
      <label>Proto entries<textarea class="codebox compact" placeholder={'proto "profile" { path: "./profile.proto" }'} value={selected.data.config.protosCode} on:input={(event) => updateConfig('protosCode', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'entry'}
      <label>Method
        <select value={selected.data.config.method} on:change={(event) => updateConfig('method', event.currentTarget.value)}>
          {#each endpointMethods as method}
            <option value={method}>{method}</option>
          {/each}
        </select>
      </label>
      <label>Path<input value={selected.data.config.path} on:input={(event) => updateConfig('path', event.currentTarget.value)} /></label>
      <div class="section-title">Rate limit</div>
      <label class="inline-check">
        <input type="checkbox" checked={selected.data.config.rateLimitEnabled} on:change={(event) => updateConfig('rateLimitEnabled', event.currentTarget.checked)} />
        Enabled
      </label>
      {#if selected.data.config.rateLimitEnabled}
        <div class="field-grid">
          <label>Limit<input type="number" value={selected.data.config.rateLimitLimit} on:input={(event) => updateConfig('rateLimitLimit', Number(event.currentTarget.value))} /></label>
          <label>Unit<input value={selected.data.config.rateLimitUnit} on:input={(event) => updateConfig('rateLimitUnit', event.currentTarget.value)} /></label>
          <label>Window ms<input type="number" value={selected.data.config.rateLimitWindowMs} on:input={(event) => updateConfig('rateLimitWindowMs', Number(event.currentTarget.value))} /></label>
        </div>
      {/if}
      <div class="section-title">Secure</div>
      <label class="inline-check">
        <input type="checkbox" checked={selected.data.config.secureEnabled} on:change={(event) => updateConfig('secureEnabled', event.currentTarget.checked)} />
        Enabled
      </label>
      {#if selected.data.config.secureEnabled}
        <label>Schemes<input value={selected.data.config.secureRules} placeholder="jwt, basic" on:input={(event) => updateConfig('secureRules', event.currentTarget.value)} /></label>
        <label>Checks<textarea class="codebox compact" placeholder="jwt.sub == id" value={selected.data.config.secureChecks} on:input={(event) => updateConfig('secureChecks', event.currentTarget.value)}></textarea></label>
        <label>Rules code<textarea class="codebox" placeholder={'JWT { secret: env.JWT_SECRET, checks: [jwt.sub == id] }'} value={selected.data.config.secureRulesCode} on:input={(event) => updateConfig('secureRulesCode', event.currentTarget.value)}></textarea></label>
      {/if}
    {:else if selected.data.config.kind === 'fetch'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>Method
        <select value={selected.data.config.method} on:change={(event) => updateConfig('method', event.currentTarget.value)}>
          {#each methods as method}
            <option value={method}>{method}</option>
          {/each}
        </select>
      </label>
      <label>URL expression<input value={selected.data.config.url} on:input={(event) => updateConfig('url', event.currentTarget.value)} /></label>
      <label>Body expression<textarea class="codebox compact" value={selected.data.config.body} on:input={(event) => updateConfig('body', event.currentTarget.value)}></textarea></label>
      <div class="field-grid">
        <label>Timeout ms<input type="number" value={selected.data.config.timeoutMs} on:input={(event) => updateConfig('timeoutMs', Number(event.currentTarget.value))} /></label>
        <label>Retries<input type="number" value={selected.data.config.retries} on:input={(event) => updateConfig('retries', Number(event.currentTarget.value))} /></label>
        <label>Delay ms<input type="number" value={selected.data.config.delayMs} on:input={(event) => updateConfig('delayMs', Number(event.currentTarget.value))} /></label>
      </div>
      <label>Fallback JSON/expression<input value={selected.data.config.fallback} on:input={(event) => updateConfig('fallback', event.currentTarget.value)} /></label>
      <label>Node code<textarea class="codebox" value={selected.data.config.code} on:input={(event) => updateConfig('code', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'let'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>Expression<textarea class="codebox" value={selected.data.config.value} on:input={(event) => updateConfig('value', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'pipe'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>Source<input value={selected.data.config.source} on:input={(event) => updateConfig('source', event.currentTarget.value)} /></label>
      <label>Pipe code<textarea class="codebox tall" value={selected.data.config.code} on:input={(event) => updateConfig('code', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'response'}
      <label>Status<input type="number" value={selected.data.config.status} on:input={(event) => updateConfig('status', Number(event.currentTarget.value))} /></label>
      <label>Body code<textarea class="codebox tall" value={selected.data.config.bodyCode} on:input={(event) => updateConfig('bodyCode', event.currentTarget.value)}></textarea></label>
      <label>Headers code<textarea class="codebox" value={selected.data.config.headersCode} on:input={(event) => updateConfig('headersCode', event.currentTarget.value)}></textarea></label>
      <label>Cookies code<textarea class="codebox" value={selected.data.config.cookiesCode} on:input={(event) => updateConfig('cookiesCode', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'db'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>DB source<input value={selected.data.config.dbSource} on:input={(event) => updateConfig('dbSource', event.currentTarget.value)} /></label>
      <label>SQL<textarea class="codebox" value={selected.data.config.sql} on:input={(event) => updateConfig('sql', event.currentTarget.value)}></textarea></label>
      <label>Params<input value={selected.data.config.params} on:input={(event) => updateConfig('params', event.currentTarget.value)} /></label>
      <label>Timeout ms<input type="number" value={selected.data.config.timeoutMs} on:input={(event) => updateConfig('timeoutMs', Number(event.currentTarget.value))} /></label>
      <label>Fallback<input value={selected.data.config.fallback} on:input={(event) => updateConfig('fallback', event.currentTarget.value)} /></label>
      <label>DB code<textarea class="codebox tall" value={selected.data.config.code} on:input={(event) => updateConfig('code', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'grpc'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>Service method<input value={selected.data.config.serviceMethod} on:input={(event) => updateConfig('serviceMethod', event.currentTarget.value)} /></label>
      <label>Proto path<input value={selected.data.config.protoPath} on:input={(event) => updateConfig('protoPath', event.currentTarget.value)} /></label>
      <div class="field-grid">
        <label>Service<input value={selected.data.config.service} on:input={(event) => updateConfig('service', event.currentTarget.value)} /></label>
        <label>Method<input value={selected.data.config.method} on:input={(event) => updateConfig('method', event.currentTarget.value)} /></label>
        <label>Timeout<input type="number" value={selected.data.config.timeoutMs} on:input={(event) => updateConfig('timeoutMs', Number(event.currentTarget.value))} /></label>
      </div>
      <label>Payload<textarea class="codebox" value={selected.data.config.payload} on:input={(event) => updateConfig('payload', event.currentTarget.value)}></textarea></label>
      <label>Fallback<input value={selected.data.config.fallback} on:input={(event) => updateConfig('fallback', event.currentTarget.value)} /></label>
      <label>gRPC code<textarea class="codebox tall" value={selected.data.config.code} on:input={(event) => updateConfig('code', event.currentTarget.value)}></textarea></label>
    {:else if selected.data.config.kind === 'command'}
      <label>Variable<input value={selected.data.config.variable} on:input={(event) => updateConfig('variable', event.currentTarget.value)} /></label>
      <label>Command<textarea class="codebox" value={selected.data.config.command} on:input={(event) => updateConfig('command', event.currentTarget.value)}></textarea></label>
    {:else}
      <label>Code<textarea class="codebox tall" value={selected.data.config.code} on:input={(event) => updateConfig('code', event.currentTarget.value)}></textarea></label>
    {/if}

    <div class="section-title">Ports</div>
    <div class="handle-list">
      {#each selected.data.handles as handle}
        <span class={handle.type}>{handle.type}: {handle.label}</span>
      {/each}
    </div>
    <button class="danger" disabled={['gateway', 'entry', 'response'].includes(selected.data.config.kind)} on:click={() => mutateSelected('delete-node')}>Delete layer</button>
  {:else if selectedEdge}
    <div class="section-title">Connection</div>
    <label>Label<input value={selectedEdge.label ?? ''} on:input={(event) => updateSelectedEdge('label', event.currentTarget.value)} /></label>
    <label>From layer
      <select value={selectedEdge.source} on:change={(event) => updateSelectedEdge('source', event.currentTarget.value)}>
        {#each nodes as node}
          <option value={node.id}>{nodeLabel(node)}</option>
        {/each}
      </select>
    </label>
    <label>From port
      <select value={selectedEdge.sourceHandle ?? ''} on:change={(event) => updateSelectedEdge('sourceHandle', event.currentTarget.value || null)}>
        <option value="">auto</option>
        {#each handleOptions(selectedEdge.source, 'out') as handle}
          <option value={handle.id}>{handle.label}</option>
        {/each}
      </select>
    </label>
    <label>To layer
      <select value={selectedEdge.target} on:change={(event) => updateSelectedEdge('target', event.currentTarget.value)}>
        {#each nodes as node}
          <option value={node.id}>{nodeLabel(node)}</option>
        {/each}
      </select>
    </label>
    <label>To port
      <select value={selectedEdge.targetHandle ?? ''} on:change={(event) => updateSelectedEdge('targetHandle', event.currentTarget.value || null)}>
        <option value="">auto</option>
        {#each handleOptions(selectedEdge.target, 'in') as handle}
          <option value={handle.id}>{handle.label}</option>
        {/each}
      </select>
    </label>
    <button class="danger" on:click={deleteSelectedEdge}>Delete connection</button>
  {:else}
    <p>Select a DAG layer or connection. Drag from an output port to an input port to create a connection; drag an existing connection endpoint to reconnect it.</p>
  {/if}
</aside>
