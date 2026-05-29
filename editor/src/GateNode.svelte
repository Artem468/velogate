<script>
  import { Handle, Position } from '@xyflow/svelte';

  export let data;

  $: inputHandles = data.handles.filter((handle) => handle.type === 'in');
  $: outputHandles = data.handles.filter((handle) => handle.type === 'out');

  function handleTop(index, total) {
    return `${((index + 1) * 100) / (total + 1)}%`;
  }
</script>

<div class={`gate-node gate-node-${data.kind}`}>
  {#each inputHandles as handle, index}
    <Handle
      id={handle.id ?? `in-${index}`}
      type="target"
      position={Position.Left}
      style={`top: ${handleTop(index, inputHandles.length)};`}
    />
  {/each}
  <div class="node-title">
    <span>{data.title}</span>
    <strong>{data.badge}</strong>
  </div>
  <div class="node-body">
    {#each data.lines as line}
      <div class="node-line">{line}</div>
    {/each}
  </div>
  <div class="handles">
    {#each data.handles as handle}
      <span class={`handle-pill ${handle.type}`}>{handle.label}</span>
    {/each}
  </div>
  {#each outputHandles as handle, index}
    <Handle
      id={handle.id ?? `out-${index}`}
      type="source"
      position={Position.Right}
      style={`top: ${handleTop(index, outputHandles.length)};`}
    />
  {/each}
</div>
