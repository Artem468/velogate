<script>
    import {addEdge} from '@xyflow/svelte';
    import {Toaster, toast} from 'svelte-sonner';
    import '@xyflow/svelte/dist/style.css';
    import FlowCanvas from './FlowCanvas.svelte';
    import GateNode from './GateNode.svelte';
    import HeaderBar from './HeaderBar.svelte';
    import InspectorPanel from './InspectorPanel.svelte';
    import Rail from './Rail.svelte';
    import SourceEditor from './SourceEditor.svelte';
    import {
        buildEndpointBody,
        buildEndpointOptionsSource,
        buildGatewayBody,
        draftConfig,
        endpointConfig,
        endpointMethods,
        fetchCode,
        formatExpr,
        gatewayConfig,
        grpcCode,
        linesFromConfig,
        methods,
        pipeCode,
        responseBodyCode,
        responseCode,
        syncNodePresentation
    } from './lib/dsl.js';
    import {allEndpointIndexes, normalizeVisibleEndpoints, setEndpointIndexVisible} from './lib/endpointVisibility.js';
    import {graphSnapshot as createGraphSnapshot, normalizeGraphPositions} from './lib/graph.js';

    let elkPromise = null;
    function getElk() {
        elkPromise ??= import('elkjs/lib/elk.bundled.js').then(({default: ELK}) => new ELK());
        return elkPromise;
    }
    const nodeTypes = {gate: GateNode};

    let state = null;
    let nodes = [];
    let edges = [];
    let selected = null;
    let selectedEdge = null;
    let source = '';
    let endpointIndex = 0;
    let mode = 'graph';
    let busy = false;
    let ws = null;
    let saveTimer = null;
    let graphSaveTimer = null;
    let endpointSaveTimer = null;
    let endpointOptionsSaveTimer = null;
    let pendingEndpointAdd = false;
    let pendingManualGraphSave = false;
    let autoSaveGraph = localStorage.getItem('velogate:auto-save-graph') !== 'false';
    let visibleEndpointIndexes = [];
    let endpointVisibilityInitialized = false;
    let lastSavedAt = '';

    function notify(type, text) {
        if (type === 'error') toast.error(text);
        else if (type === 'success') toast.success(text);
        else toast(text);
    }

    function connectWs() {
        const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
        ws = new WebSocket(`${scheme}://${location.host}/api/editor/ws`);
        ws.onmessage = (event) => {
            const message = JSON.parse(event.data);
            if (message.kind === 'endpoint_added' || message.kind === 'endpoint_updated' || message.kind === 'endpoint_options_updated' || message.kind === 'endpoint_graph_saved' || message.kind === 'gateway_updated') {
                const preserveGraphState = graphSnapshot();
                state = message.state;
                source = state.model.source;
                if (message.kind === 'endpoint_added' || pendingEndpointAdd) {
                    endpointIndex = Math.max((state.model.parsed?.file.endpoints.length ?? 1) - 1, 0);
                    pendingEndpointAdd = false;
                    syncVisibleEndpoints(true);
                } else {
                    syncVisibleEndpoints(false);
                }
                lastSavedAt = new Date().toLocaleTimeString();
                rebuildGraph({snapshot: preserveGraphState});
                if (message.kind === 'endpoint_added') {
                    notify('success', 'Endpoint added to .gate');
                } else if (message.kind === 'endpoint_updated') {
                    notify('success', 'Endpoint saved to .gate');
                } else if (message.kind === 'gateway_updated' && pendingManualGraphSave) {
                    notify('success', 'Gateway saved to .gate');
                } else if (pendingManualGraphSave) {
                    notify('success', 'Graph saved to .gate');
                }
                pendingManualGraphSave = false;
            }
            if (message.kind === 'error') {
                pendingManualGraphSave = false;
                notify('error', message.error);
            }
        };
        ws.onclose = () => setTimeout(connectWs, 800);
    }

    async function loadState() {
        const response = await fetch('/api/editor/state');
        state = await response.json();
        if (!response.ok) {
            notify('error', state.error ?? 'Failed to load editor state');
            return;
        }
        source = state.model.source;
        endpointIndex = Math.min(endpointIndex, Math.max((state.model.parsed?.file.endpoints.length ?? 1) - 1, 0));
        syncVisibleEndpoints(false);
        await rebuildGraph();
        connectWs();
    }

    function endpointList() {
        return state?.model.parsed?.file.endpoints ?? [];
    }

    function syncVisibleEndpoints(includeLast) {
        const count = endpointList().length;
        visibleEndpointIndexes = normalizeVisibleEndpoints(visibleEndpointIndexes, count, {
            initialized: endpointVisibilityInitialized,
            includeLast
        });
        endpointVisibilityInitialized = true;
    }

    function isEndpointVisible(index) {
        return visibleEndpointIndexes.includes(index);
    }

    function setEndpointVisible(index, visible) {
        visibleEndpointIndexes = setEndpointIndexVisible(visibleEndpointIndexes, index, visible);
        selected = null;
        selectedEdge = null;
        rebuildGraph();
    }

    function showAllEndpoints() {
        visibleEndpointIndexes = allEndpointIndexes(endpointList().length);
        endpointVisibilityInitialized = true;
        selected = null;
        selectedEdge = null;
        rebuildGraph();
    }

    function hideAllEndpoints() {
        visibleEndpointIndexes = [];
        endpointVisibilityInitialized = true;
        selected = null;
        selectedEdge = null;
        rebuildGraph();
    }

    function styleEdge(edge) {
        const active = selectedEdge?.id === edge.id;
        return {
            ...edge,
            type: edge.type ?? 'smoothstep',
            animated: edge.animated ?? edge.target !== 'response',
            selectable: true,
            deletable: true,
            focusable: true,
            interactionWidth: 28,
            style: active ? 'stroke:#f6c85f;stroke-width:3px' : 'stroke:#6d86a3;stroke-width:2px',
            labelStyle: active ? 'fill:#f6c85f;font-weight:700' : 'fill:#9eb4c7'
        };
    }

    function setEdges(nextEdges) {
        edges = nextEdges.map(styleEdge);
        selectedEdge = selectedEdge ? (edges.find((edge) => edge.id === selectedEdge.id) ?? null) : null;
    }

    function graphSnapshot() {
        return createGraphSnapshot(nodes, selected, selectedEdge);
    }

    function edgeId(source, target, sourceHandle = null, targetHandle = null) {
        return `edge-${source}${sourceHandle ? `-${sourceHandle}` : ''}-${target}${targetHandle ? `-${targetHandle}` : ''}-${Date.now()}`;
    }

    function stepNode(step, index, planStep) {
        if (step.kind === 'fetch_http') {
            const config = {
                kind: 'fetch',
                variable: step.var_name,
                method: step.config.method ?? 'GET',
                url: formatExpr(step.config.url),
                body: step.config.body ? formatExpr(step.config.body) : '',
                timeoutMs: step.config.timeout_ms ?? 500,
                retries: step.config.retries ?? 0,
                delayMs: step.config.delay_ms ?? 0,
                fallback: step.config.fallback ? formatExpr(step.config.fallback) : '',
                code: fetchCode(step)
            };
            return {
                title: `Fetch: ${config.url}`,
                badge: step.var_name,
                kind: 'fetch',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'request', type: 'in', label: 'request'}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        if (step.kind === 'pipe') {
            const config = {
                kind: 'pipe',
                variable: step.var_name,
                source: formatExpr(step.source),
                code: pipeCode(step)
            };
            return {
                title: `Transform: ${step.var_name}`,
                badge: step.var_name,
                kind: 'pipe',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'source', type: 'in', label: config.source}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        if (step.kind === 'query_db') {
            const config = {
                kind: 'db',
                variable: step.var_name,
                dbSource: formatExpr(step.config.db_source),
                sql: step.config.sql,
                params: step.config.params.map(formatExpr).join(', '),
                timeoutMs: step.config.timeout_ms ?? 500,
                fallback: step.config.fallback ? formatExpr(step.config.fallback) : '',
                code: `let ${step.var_name} = db::query(${formatExpr(step.config.db_source)}, ${JSON.stringify(step.config.sql)}${step.config.params.length ? `, ${step.config.params.map(formatExpr).join(', ')}` : ''}) {\n  timeout: ${step.config.timeout_ms ?? 500}ms\n};`
            };
            return {
                title: `DB: ${step.var_name}`,
                badge: step.var_name,
                kind: 'db',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'deps', type: 'in', label: 'params'}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        if (step.kind === 'call_grpc') {
            const config = {
                kind: 'grpc',
                variable: step.var_name,
                serviceMethod: step.config.service_method,
                protoPath: step.config.proto_path ?? '',
                service: step.config.service ?? '',
                method: step.config.method ?? '',
                payload: formatExpr(step.config.payload),
                timeoutMs: step.config.timeout_ms ?? 500,
                fallback: step.config.fallback ? formatExpr(step.config.fallback) : '',
                code: grpcCode(step)
            };
            return {
                title: `gRPC: ${step.var_name}`,
                badge: step.var_name,
                kind: 'grpc',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'payload', type: 'in', label: 'payload'}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        if (step.kind === 'command') {
            const config = {
                kind: 'command',
                variable: step.var_name,
                command: step.command,
                code: `command ${JSON.stringify(step.command)} as ${step.var_name};`
            };
            return {
                title: `Command: ${step.var_name}`,
                badge: step.var_name,
                kind: 'command',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'deps', type: 'in', label: 'deps'}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        if (step.kind === 'let') {
            const config = {
                kind: 'let',
                variable: step.var_name,
                value: formatExpr(step.value),
                code: `let ${step.var_name} = ${formatExpr(step.value)};`
            };
            return {
                title: `Let: ${step.var_name}`,
                badge: step.var_name,
                kind: 'let',
                config,
                lines: linesFromConfig(config),
                handles: [{id: 'deps', type: 'in', label: 'deps'}, {
                    id: step.var_name,
                    type: 'out',
                    label: step.var_name
                }],
                raw: step,
                plan: planStep,
                index
            };
        }
        const config = {
            kind: 'step',
            variable: step.var_name,
            code: JSON.stringify(step, null, 2),
            lines: [`Reads: ${planStep?.reads?.join(', ') || 'none'}`]
        };
        return {
            title: `${step.kind}: ${step.var_name}`,
            badge: step.var_name,
            kind: 'step',
            config,
            lines: linesFromConfig(config),
            handles: [{id: 'deps', type: 'in', label: 'deps'}, {id: step.var_name, type: 'out', label: step.var_name}],
            raw: step,
            plan: planStep,
            index
        };
    }

    async function rebuildGraph({snapshot = null} = {}) {
        if (!state?.model.parsed) {
            nodes = [];
            edges = [];
            selected = null;
            selectedEdge = null;
            return;
        }

        const graphNodes = [];
        const graphEdges = [];
        const gateway = state.model.parsed.file.gateway;
        const gatewayNodeConfig = gatewayConfig(gateway);
        gatewayNodeConfig.lines = linesFromConfig(gatewayNodeConfig);
        graphNodes.push({
            id: 'gateway',
            width: 330,
            height: 190,
            data: {
                title: `Gateway: ${gateway.name}`,
                badge: 'root',
                kind: 'gateway',
                config: gatewayNodeConfig,
                lines: gatewayNodeConfig.lines,
                handles: [{id: 'gateway', type: 'out', label: 'gateway'}],
                raw: gateway
            }
        });

        for (const [epIndex, endpoint] of state.model.parsed.file.endpoints.entries()) {
            if (!isEndpointVisible(epIndex)) continue;
            const plan = state.model.parsed.plan.endpoints[epIndex];
            const entryId = `endpoint-${epIndex}`;
            const responseId = `e${epIndex}-response`;
            const entryConfig = endpointConfig(endpoint, epIndex);
            entryConfig.lines = linesFromConfig(entryConfig);

            graphNodes.push({
                id: entryId,
                width: 330,
                height: 178,
                data: {
                    title: `${endpoint.method} ${endpoint.path}`,
                    badge: 'endpoint',
                    kind: 'entry',
                    endpointIndex: epIndex,
                    config: entryConfig,
                    lines: entryConfig.lines,
                    handles: [{id: 'request', type: 'in', label: 'gateway'}, {id: 'request', type: 'out', label: 'request'}],
                    raw: endpoint
                }
            });
            graphEdges.push({
                id: `gateway-${entryId}`,
                source: 'gateway',
                target: entryId,
                label: `${endpoint.method} ${endpoint.path}`
            });

            endpoint.steps.forEach((step, index) => {
                const data = stepNode(step, index, plan.steps[index]);
                data.endpointIndex = epIndex;
                graphNodes.push({id: `e${epIndex}-step-${index}`, width: 320, height: 196, data});
                const deps = plan.steps[index]?.dependencies ?? [];
                if (deps.length === 0) graphEdges.push({
                    id: `${entryId}-step-${index}`,
                    source: entryId,
                    target: `e${epIndex}-step-${index}`,
                    label: 'request'
                });
                deps.forEach((dep) => graphEdges.push({
                    id: `e${epIndex}-step-${dep}-step-${index}`,
                    source: `e${epIndex}-step-${dep}`,
                    target: `e${epIndex}-step-${index}`,
                    label: plan.steps[dep]?.produces
                }));
            });

            const responseConfig = {
                kind: 'response',
                endpointIndex: epIndex,
                status: endpoint.response_status,
                bodyCode: responseBodyCode(endpoint.response_body),
                headersCode: Object.keys(endpoint.response_headers ?? {}).length ? responseBodyCode(endpoint.response_headers) : '',
                cookiesCode: Object.keys(endpoint.response_cookies ?? {}).length ? responseBodyCode(endpoint.response_cookies) : '',
                code: responseCode(endpoint)
            };
            graphNodes.push({
                id: responseId,
                width: 300,
                height: 190,
                data: {
                    title: `Respond: ${endpoint.response_status}`,
                    badge: 'finish',
                    kind: 'response',
                    endpointIndex: epIndex,
                    config: responseConfig,
                    lines: linesFromConfig(responseConfig),
                    handles: [{id: 'response', type: 'in', label: plan.response_reads.join(', ') || 'response'}],
                    raw: endpoint.response_body
                }
            });

            if (endpoint.steps.length === 0) {
                graphEdges.push({id: `${entryId}-${responseId}`, source: entryId, target: responseId, label: 'respond'});
            } else {
                const producers = new Map(plan.steps.map((step) => [step.produces, step.index]));
                const responseDeps = plan.response_reads.map((read) => producers.get(read)).filter((idx) => idx !== undefined);
                const lastDeps = responseDeps.length ? responseDeps : [endpoint.steps.length - 1];
                lastDeps.forEach((idx) => graphEdges.push({
                    id: `e${epIndex}-step-${idx}-response`,
                    source: `e${epIndex}-step-${idx}`,
                    target: responseId,
                    label: plan.steps[idx]?.produces
                }));
            }
        }

        const elkGraph = {
            id: 'root',
            layoutOptions: {
                'elk.algorithm': 'layered',
                'elk.direction': 'RIGHT',
                'elk.layered.spacing.nodeNodeBetweenLayers': '54',
                'elk.spacing.nodeNode': '38'
            },
            children: graphNodes.map(({id, width, height}) => ({id, width, height})),
            edges: graphEdges.map((edge) => ({id: edge.id, sources: [edge.source], targets: [edge.target]}))
        };
        const elk = await getElk();
        const layout = await elk.layout(elkGraph);
        const positions = new Map(layout.children.map((node) => [node.id, {x: node.x, y: node.y}]));
        const nextNodes = graphNodes.map((node) => ({
            ...node,
            type: 'gate',
            position: snapshot?.positions?.get(node.id) ?? positions.get(node.id) ?? {x: 0, y: 0}
        }));
        nodes = snapshot ? nextNodes : normalizeGraphPositions(nextNodes);
        selected = snapshot?.selectedId ? (nodes.find((node) => node.id === snapshot.selectedId) ?? null) : null;
        selectedEdge = snapshot?.selectedEdgeId ? {id: snapshot.selectedEdgeId} : null;
        setEdges(graphEdges.map((edge) => ({...edge, type: 'smoothstep', animated: edge.target !== 'response'})));
        markSavedLocally();
    }

    function relayoutGraph() {
        nodes = normalizeGraphPositions(nodes);
        markSavedLocally();
    }

    function markSavedLocally() {
        clearTimeout(saveTimer);
        saveTimer = setTimeout(() => {
            lastSavedAt = new Date().toLocaleTimeString();
        }, 180);
    }

    function queueSave() {
        markSavedLocally();
        if (!autoSaveGraph) return;
        clearTimeout(graphSaveTimer);
        graphSaveTimer = setTimeout(() => saveCurrentGraph(true), 700);
    }

    function toggleAutoSave() {
        autoSaveGraph = !autoSaveGraph;
        localStorage.setItem('velogate:auto-save-graph', autoSaveGraph ? 'true' : 'false');
        notify('success', autoSaveGraph ? 'Autosave enabled' : 'Autosave disabled');
    }

    function sendWs(message) {
        if (!ws || ws.readyState !== WebSocket.OPEN) {
            notify('error', 'WebSocket is not connected yet');
            return false;
        }
        ws.send(JSON.stringify(message));
        return true;
    }

    function endpointIndexForSelection() {
        if (selected?.data?.endpointIndex !== undefined) return selected.data.endpointIndex;
        if (selectedEdge) {
            const source = nodes.find((node) => node.id === selectedEdge.source);
            const target = nodes.find((node) => node.id === selectedEdge.target);
            return source?.data?.endpointIndex ?? target?.data?.endpointIndex ?? null;
        }
        return visibleEndpointIndexes[0] ?? endpointIndex ?? null;
    }

    function queueEndpointSave(config) {
        clearTimeout(endpointSaveTimer);
        endpointSaveTimer = setTimeout(() => {
            sendWs({
                kind: 'endpoint_update',
                endpoint_index: config.endpointIndex ?? 0,
                method: config.method,
                path: config.path
            });
        }, 350);
    }

    function queueEndpointOptionsSave(config) {
        clearTimeout(endpointOptionsSaveTimer);
        endpointOptionsSaveTimer = setTimeout(() => {
            const optionsSource = buildEndpointOptionsSource(config, notify, true);
            if (optionsSource === null) return;
            sendWs({
                kind: 'endpoint_options_update',
                endpoint_index: config.endpointIndex ?? 0,
                options_source: optionsSource
            });
        }, 500);
    }

    function saveCurrentGraph(silent = false) {
        if (mode !== 'graph') return;
        const context = selected?.data?.config?.kind === 'gateway' ? 'gateway' : endpointIndexForSelection();
        if (context === 'gateway') {
            const gatewaySource = buildGatewayBody(nodes, notify, silent);
            if (!gatewaySource) return;
            const gatewayName = nodes.find((node) => node.id === 'gateway')?.data?.config?.name ?? 'gateway';
            pendingManualGraphSave = !silent;
            const sent = sendWs({kind: 'gateway_update', gateway_name: gatewayName, gateway_source: gatewaySource});
            if (!sent) pendingManualGraphSave = false;
            return;
        }
        if (context === null) {
            if (!silent) notify('error', 'Select an endpoint or gateway to save');
            return;
        }
        const body = buildEndpointBody(nodes, context, notify, silent);
        if (!body) return;
        pendingManualGraphSave = !silent;
        const sent = sendWs({kind: 'endpoint_graph_save', endpoint_index: context, endpoint_source: body});
        if (!sent) pendingManualGraphSave = false;
    }

    async function control(action) {
        busy = true;
        const response = await fetch('/api/editor/control', {
            method: 'POST',
            headers: {'content-type': 'application/json'},
            body: JSON.stringify({action})
        });
        state = await response.json();
        notify(response.ok ? 'success' : 'error', response.ok ? `Runtime: ${action}` : state.error);
        busy = false;
    }

    async function saveConfig() {
        busy = true;
        const response = await fetch('/api/editor/config', {
            method: 'PUT',
            headers: {'content-type': 'application/json'},
            body: JSON.stringify({source})
        });
        state = await response.json();
        notify(response.ok ? 'success' : 'error', response.ok ? 'Config saved and runtime reloaded' : state.error);
        busy = false;
        syncVisibleEndpoints(false);
        await rebuildGraph();
    }

    function selectNode(node) {
        selectedEdge = null;
        selected = nodes.find((item) => item.id === node.id) ?? node;
        if (selected?.data?.endpointIndex !== undefined) endpointIndex = selected.data.endpointIndex;
        setEdges(edges);
    }

    function selectEdge(edge) {
        selected = null;
        selectedEdge = edges.find((item) => item.id === edge.id) ?? edge;
        setEdges(edges);
    }

    function clearSelection() {
        selected = null;
        selectedEdge = null;
        setEdges(edges);
    }

    function updateSelected(updater, save = true, changedField = null) {
        if (!selected) return;
        nodes = nodes.map((node) => {
            if (node.id !== selected.id) return node;
            const next = {
                ...node,
                data: {
                    ...node.data,
                    config: {...node.data.config},
                    handles: [...node.data.handles],
                    lines: [...node.data.lines]
                }
            };
            updater(next);
            syncNodePresentation(next, changedField);
            selected = next;
            return next;
        });
        if (save) queueSave();
    }

    function updateConfig(field, value) {
        const kind = selected?.data?.config?.kind;
        const endpointHeaderFields = new Set(['method', 'path']);
        const endpointOptionFields = new Set([
            'rateLimitEnabled',
            'rateLimitLimit',
            'rateLimitUnit',
            'rateLimitWindowMs',
            'secureEnabled',
            'secureRules',
            'secureChecks',
            'secureRulesCode'
        ]);
        const saveMode = kind === 'entry' && (endpointHeaderFields.has(field) || endpointOptionFields.has(field))
            ? false
            : true;
        updateSelected((node) => {
            node.data.config[field] = value;
            if (node.data.config.kind === 'entry' && endpointHeaderFields.has(field)) {
                queueEndpointSave(node.data.config);
            } else if (node.data.config.kind === 'entry' && endpointOptionFields.has(field)) {
                queueEndpointOptionsSave(node.data.config);
            }
        }, saveMode, field);
    }

    function mutateSelected(kind) {
        if (!selected) return;
        const node = nodes.find((item) => item.id === selected.id);
        if (!node) return;
        if (kind === 'delete-node' && !['gateway', 'entry', 'response'].includes(node.data?.kind)) {
            nodes = nodes.filter((item) => item.id !== node.id);
            setEdges(edges.filter((edge) => edge.source !== node.id && edge.target !== node.id));
            selected = null;
            notify('success', 'Node removed');
            queueSave();
            return;
        }
    }

    function onConnect(connection) {
        if (!connection.source || !connection.target || connection.source === connection.target) return;
        const nextEdge = {
            ...connection,
            id: edgeId(connection.source, connection.target, connection.sourceHandle, connection.targetHandle),
            label: connection.sourceHandle || 'flow',
            type: 'smoothstep',
            animated: true
        };
        const nextEdges = addEdge(nextEdge, edges);
        setEdges(nextEdges);
        selected = null;
        selectedEdge = nextEdges.find((edge) => edge.id === nextEdge.id) ?? null;
        setEdges(edges);
        queueSave();
    }

    function onReconnect(oldEdge, newConnection) {
        if (!newConnection.source || !newConnection.target || newConnection.source === newConnection.target) return;
        setEdges(edges.map((edge) => edge.id === oldEdge.id
            ? {
                ...edge,
                source: newConnection.source,
                target: newConnection.target,
                sourceHandle: newConnection.sourceHandle,
                targetHandle: newConnection.targetHandle,
                label: newConnection.sourceHandle || edge.label || 'flow'
            }
            : edge));
        selectedEdge = edges.find((edge) => edge.id === oldEdge.id) ?? null;
        setEdges(edges);
        queueSave();
    }

    function deleteSelectedEdge() {
        if (!selectedEdge) return;
        const deleted = selectedEdge;
        selectedEdge = null;
        setEdges(edges.filter((edge) => edge.id !== deleted.id));
        notify('success', 'Connection removed');
        queueSave();
    }

    function updateSelectedEdge(field, value) {
        if (!selectedEdge) return;
        if ((field === 'source' && value === selectedEdge.target) || (field === 'target' && value === selectedEdge.source)) {
            notify('error', 'Connection cannot point to the same layer');
            return;
        }
        setEdges(edges.map((edge) => edge.id === selectedEdge.id ? {...edge, [field]: value} : edge));
        selectedEdge = edges.find((edge) => edge.id === selectedEdge.id) ?? null;
        queueSave();
    }

    function nodeLabel(node) {
        return node?.data?.title || node?.id || 'layer';
    }

    function handleOptions(nodeId, type) {
        return nodes.find((node) => node.id === nodeId)?.data?.handles?.filter((handle) => handle.type === type) ?? [];
    }

    function nextDraftPosition() {
        const targetEndpointIndex = endpointIndexForSelection();
        const anchor = selected?.data?.endpointIndex === targetEndpointIndex
            ? selected
            : nodes.find((node) => node.data?.kind === 'entry' && node.data?.endpointIndex === targetEndpointIndex);
        if (anchor) {
            const siblings = nodes.filter((node) => Math.abs((node.position?.x ?? 0) - ((anchor.position?.x ?? 0) + 400)) < 120).length;
            return {
                x: Math.round((anchor.position?.x ?? 0) + 400),
                y: Math.round((anchor.position?.y ?? 0) + siblings * 230)
            };
        }
        return {x: 80, y: 80};
    }

    function autoConnectDraft(draft) {
        if (!selected || selected.data?.kind === 'response' || selected.data?.kind === 'gateway') return;
        if (selected.data?.endpointIndex !== draft.data?.endpointIndex) return;
        const sourceHandle = selected.data.handles.find((handle) => handle.type === 'out')?.id ?? null;
        const targetHandle = draft.data.handles.find((handle) => handle.type === 'in')?.id ?? null;
        const nextEdge = {
            id: edgeId(selected.id, draft.id, sourceHandle, targetHandle),
            source: selected.id,
            target: draft.id,
            sourceHandle,
            targetHandle,
            label: sourceHandle || 'flow',
            type: 'smoothstep',
            animated: true
        };
        setEdges(addEdge(nextEdge, edges));
    }

    function addDraftNode(kind) {
        if (selected?.data?.kind === 'gateway') {
            notify('error', 'Select an endpoint before adding a layer');
            return;
        }
        const targetEndpointIndex = endpointIndexForSelection();
        if (targetEndpointIndex === null) {
            notify('error', 'Select an endpoint before adding a layer');
            return;
        }
        const index = nodes.filter((node) => node.id.startsWith('draft-')).length + 1;
        const config = draftConfig(kind, index);
        const draft = {
            id: `draft-${kind}-${Date.now()}`,
            type: 'gate',
            position: nextDraftPosition(),
            data: {
                title: '',
                badge: '',
                kind,
                endpointIndex: targetEndpointIndex,
                config,
                lines: [],
                handles: [],
                raw: {draft: true, kind}
            }
        };
        draft.data.config.endpointIndex = targetEndpointIndex;
        syncNodePresentation(draft);
        const previousSelection = selected;
        nodes = [...nodes, draft];
        selected = previousSelection;
        autoConnectDraft(draft);
        selected = draft;
        notify('success', `${kind.toUpperCase()} layer added`);
        queueSave();
    }

    function addEndpoint() {
        const method = (window.prompt('HTTP method', 'GET') ?? '').trim().toUpperCase();
        if (!method) return;
        const path = (window.prompt('Endpoint path', '') ?? '').trim();
        if (!path) return;
        pendingEndpointAdd = true;
        sendWs({kind: 'endpoint_add', method, path});
    }

    function onKeydown(event) {
        if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
            event.preventDefault();
            if (mode === 'source') saveConfig();
            else saveCurrentGraph(false);
        }
    }

    function setMode(nextMode) {
        mode = nextMode;
    }

    function onFlowDelete({edges: deletedEdges}) {
        if (deletedEdges.some((edge) => edge.id === selectedEdge?.id)) selectedEdge = null;
        queueSave();
    }

    window.addEventListener('keydown', onKeydown);
    loadState();
</script>

<Toaster
        richColors
        position="top-right"
        theme="dark"
        toastOptions={{ style: 'background:#101820;border:1px solid #2d3a48;color:#e5eef6' }}
/>

<main>
    <Rail
            {mode}
            {busy}
            {autoSaveGraph}
            setMode={setMode}
            saveGraph={saveCurrentGraph}
            toggleAutoSave={toggleAutoSave}
            control={control}
    />

    <section class="workspace">
        <HeaderBar
                {state}
                {busy}
                {mode}
                {lastSavedAt}
                {visibleEndpointIndexes}
                addEndpoint={addEndpoint}
                addDraftNode={addDraftNode}
                relayoutGraph={relayoutGraph}
                setEndpointVisible={setEndpointVisible}
                showAllEndpoints={showAllEndpoints}
                hideAllEndpoints={hideAllEndpoints}
        />

        {#if state?.model.error}
            <div class="error">{state.model.error}</div>
        {/if}
        {#if mode === 'graph'}
            <FlowCanvas
                    bind:nodes
                    bind:edges
                    {nodeTypes}
                    selectNode={selectNode}
                    selectEdge={selectEdge}
                    clearSelection={clearSelection}
                    onConnect={onConnect}
                    onReconnect={onReconnect}
                    onDelete={onFlowDelete}
            />
        {:else}
            <SourceEditor bind:source {busy} saveConfig={saveConfig} />
        {/if}
    </section>

    <InspectorPanel
            {selected}
            {selectedEdge}
            {nodes}
            {methods}
            {endpointMethods}
            updateConfig={updateConfig}
            mutateSelected={mutateSelected}
            updateSelectedEdge={updateSelectedEdge}
            deleteSelectedEdge={deleteSelectedEdge}
            nodeLabel={nodeLabel}
            handleOptions={handleOptions}
    />
</main>
