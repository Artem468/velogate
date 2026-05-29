<script>
  import { Controls, SvelteFlow } from '@xyflow/svelte';

  export let nodes = [];
  export let edges = [];
  export let nodeTypes;
  export let selectNode;
  export let selectEdge;
  export let clearSelection;
  export let onConnect;
  export let onReconnect;
  export let onDelete;
</script>

<div class="flow">
  <SvelteFlow
    bind:nodes
    bind:edges
    {nodeTypes}
    initialViewport={{ x: 0, y: 0, zoom: 0.78 }}
    colorMode="dark"
    nodesDraggable={true}
    nodesConnectable={true}
    elementsSelectable={true}
    edgesFocusable={true}
    elevateEdgesOnSelect={true}
    deleteKey={['Backspace', 'Delete']}
    onnodeclick={({ node }) => selectNode(node)}
    onedgeclick={({ edge }) => selectEdge(edge)}
    onpaneclick={clearSelection}
    onconnect={onConnect}
    onreconnect={onReconnect}
    ondelete={onDelete}
  >
    <Controls />
  </SvelteFlow>
</div>
