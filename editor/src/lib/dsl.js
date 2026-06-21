import {formatExpr, trimValue} from './expressions.js';
import {pipeCode, syncPipeConfig} from './pipe.js';

export {formatExpr, trimValue} from './expressions.js';
export {pipeCode} from './pipe.js';

export const methods = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];
export const endpointMethods = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE'];

export function fetchCode(step) {
    const c = step.config;
    return [
        `fetch ${formatExpr(c.url)} as ${step.var_name} {`,
        `  method: "${c.method ?? 'GET'}",`,
        c.body ? `  body: ${formatExpr(c.body)},` : null,
        `  timeout: ${c.timeout_ms ?? 500}ms,`,
        `  retry: ${c.retries ?? 0} times,`,
        c.delay_ms ? `  delay: ${c.delay_ms}ms,` : null,
        c.fallback ? `  fallback: ${formatExpr(c.fallback)}` : null,
        '};'
    ].filter(Boolean).join('\n');
}

export function grpcCode(step) {
    const c = step.config;
    const args = c.proto_path || c.service || c.method
        ? [JSON.stringify(c.service_method), JSON.stringify(c.proto_path ?? ''), JSON.stringify(c.service ?? ''), JSON.stringify(c.method ?? ''), formatExpr(c.payload)]
        : [JSON.stringify(c.service_method), formatExpr(c.payload)];
    return `let ${step.var_name} = grpc::call(${args.join(', ')}) {\n  timeout: ${c.timeout_ms ?? 500}ms\n};`;
}

export function responseBodyCode(body) {
    const entries = Object.entries(body ?? {});
    if (!entries.length) return '{}';
    return `{\n${entries.map(([key, value]) => `  ${JSON.stringify(key)}: ${formatExpr(value)}`).join(',\n')}\n}`;
}

export function responseCode(endpoint) {
    return `respond ${endpoint.response_status} ${responseBodyCode(endpoint.response_body)}`;
}

export function linesFromConfig(config) {
    if (config.kind === 'gateway') {
        const lines = [
            `Port: ${config.port || 0}`,
            `Host: ${config.host || 'default'}`,
            `Env: ${config.envFile || 'none'}`
        ];
        if (trimValue(config.corsCode)) lines.push('CORS configured');
        if (trimValue(config.constantsCode)) lines.push('Constants configured');
        if (trimValue(config.databasesCode)) lines.push('Databases configured');
        if (trimValue(config.protosCode)) lines.push('Protos configured');
        return lines;
    }
    if (config.kind === 'entry') {
        const lines = [];
        if (config.rateLimitEnabled) lines.push(`Rate Limit: ${config.rateLimitLimit} ${config.rateLimitUnit} (${config.rateLimitWindowMs}ms)`);
        if (config.secureEnabled) lines.push(`Secure: ${config.secureRules || 'scheme'}`);
        return lines;
    }
    if (config.kind === 'fetch') {
        return [
            `Method: ${config.method}`,
            `URL: ${config.url}`,
            trimValue(config.body) ? `Body: ${config.body}` : null,
            `Timeout: ${config.timeoutMs}ms`,
            `Retry: ${config.retries} times`,
            `Fallback: ${config.fallback || 'none'}`
        ].filter(Boolean);
    }
    if (config.kind === 'pipe') {
        return [`Source: ${config.source}`, ...config.code.split('\n').filter(Boolean).slice(1, 5)];
    }
    if (config.kind === 'response') {
        return [`Status: ${config.status}`, ...config.bodyCode.split('\n').slice(0, 4)];
    }
    if (config.kind === 'db') {
        return [
            `DB: ${config.dbSource}`,
            `SQL: ${config.sql}`,
            `Timeout: ${config.timeoutMs}ms`,
            `Fallback: ${config.fallback || 'none'}`
        ];
    }
    if (config.kind === 'grpc') {
        return [
            `Method: ${config.serviceMethod}`,
            `Proto: ${config.protoPath || 'static proto'}`,
            `Timeout: ${config.timeoutMs}ms`,
            `Fallback: ${config.fallback || 'none'}`
        ];
    }
    if (config.kind === 'command') {
        return [`Command: ${config.command || 'empty'}`];
    }
    if (config.kind === 'let') {
        return [`Value: ${config.value || 'empty'}`];
    }
    return config.lines ?? [];
}

export function gatewayConfig(gateway) {
    return {
        kind: 'gateway',
        name: gateway.name ?? 'VeloGate',
        port: gateway.port ?? 0,
        host: gateway.host ?? '',
        envFile: gateway.env_file ?? '',
        corsCode: formatCorsCode(gateway.cors),
        constantsCode: Object.entries(gateway.constants ?? {})
            .map(([key, value]) => `${JSON.stringify(key)}: ${formatExpr(value)}`)
            .join(',\n'),
        databasesCode: (gateway.static_dbs ?? [])
            .map((db) => `db ${JSON.stringify(db.name)} { url: ${JSON.stringify(db.url)} }`)
            .join('\n'),
        protosCode: (gateway.static_protos ?? [])
            .map((proto) => `proto ${JSON.stringify(proto.name)} { path: ${JSON.stringify(proto.path)} }`)
            .join('\n'),
        lines: []
    };
}

function formatCorsCode(cors) {
    if (!cors) return '';
    const lines = [];
    if (cors.origins?.length) lines.push(`origins: ${JSON.stringify(cors.origins)}`);
    if (cors.methods?.length) lines.push(`methods: ${JSON.stringify(cors.methods)}`);
    if (cors.headers?.length) lines.push(`headers: ${JSON.stringify(cors.headers)}`);
    if (cors.expose_headers?.length) lines.push(`expose_headers: ${JSON.stringify(cors.expose_headers)}`);
    if (cors.credentials) lines.push(`credentials: true`);
    if (cors.max_age_seconds != null) lines.push(`max_age: ${Number(cors.max_age_seconds)}`);
    return lines.join(',\n');
}

export function endpointConfig(endpoint, index) {
    const rateLimit = endpoint.options.find((option) => option.kind === 'rate_limit');
    const secure = endpoint.options.find((option) => option.kind === 'secure');
    const secureRules = secure?.rules?.map((rule) => rule.scheme).join(', ') ?? '';
    const secureChecks = secure?.rules?.flatMap((rule) => rule.checks ?? []).map(formatExpr).join('\n') ?? '';
    const secureRulesCode = secure?.rules?.map(formatSecureRule).join(',\n') ?? '';
    return {
        kind: 'entry',
        endpointIndex: index,
        method: endpoint.method,
        path: endpoint.path,
        rateLimitEnabled: Boolean(rateLimit),
        rateLimitLimit: rateLimit?.limit ?? 100,
        rateLimitUnit: rateLimit?.unit ?? 'rps',
        rateLimitWindowMs: rateLimit?.window_ms ?? 1000,
        secureEnabled: Boolean(secure),
        secureRules,
        secureChecks,
        secureRulesCode,
        code: `${endpoint.method} ${endpoint.path}`,
        lines: []
    };
}

function formatSecureRule(rule) {
    const fields = [];
    if (rule.secret) fields.push(`secret: ${formatExpr(rule.secret)}`);
    if (rule.username) fields.push(`username: ${formatExpr(rule.username)}`);
    if (rule.password) fields.push(`password: ${formatExpr(rule.password)}`);
    if (rule.checks?.length) fields.push(`checks: [${rule.checks.map(formatExpr).join(', ')}]`);
    if (!fields.length) return rule.scheme;
    return `${rule.scheme} { ${fields.join(', ')} }`;
}

export function buildGatewayBody(nodes, notify, silent = false) {
    const errors = [];
    const config = nodes.find((node) => node.id === 'gateway')?.data?.config;
    if (!config) {
        if (!silent) notify('error', 'Gateway node not found');
        return '';
    }
    if (!trimValue(config.name)) errors.push('Gateway name is required');
    const port = Number(config.port);
    if (!Number.isInteger(port) || port < 0 || port > 65535) errors.push('Gateway port must be 0..65535');
    const lines = [`port: ${port},`];
    if (trimValue(config.host)) lines.push(`host: ${JSON.stringify(trimValue(config.host))},`);
    if (trimValue(config.envFile)) lines.push(`env_file: ${JSON.stringify(trimValue(config.envFile))},`);
    if (trimValue(config.corsCode)) lines.push(`cors: {\n${indentBlock(config.corsCode, 8)}\n    },`);
    if (trimValue(config.constantsCode)) lines.push(`constants: {\n${indentBlock(config.constantsCode, 8)}\n    },`);
    if (trimValue(config.databasesCode)) lines.push(`databases: [\n${indentBlock(config.databasesCode, 8)}\n    ],`);
    if (trimValue(config.protosCode)) lines.push(`protos: [\n${indentBlock(config.protosCode, 8)}\n    ],`);
    if (errors.length) {
        if (!silent) notify('error', errors[0]);
        return '';
    }
    return lines.map((line) => line.split('\n').map((part) => `    ${part}`).join('\n')).join('\n');
}

function indentBlock(value, spaces) {
    const prefix = ' '.repeat(spaces);
    return trimValue(value).split('\n').map((line) => `${prefix}${line}`).join('\n');
}

export function generateEndpointOptions(entryConfig, errors) {
    const lines = [];
    if (entryConfig?.rateLimitEnabled) {
        const limit = Number(entryConfig.rateLimitLimit);
        const windowMs = Number(entryConfig.rateLimitWindowMs);
        const unit = trimValue(entryConfig.rateLimitUnit) || 'rps';
        if (!Number.isInteger(limit) || limit <= 0) errors.push('Rate limit must be a positive integer');
        if (!Number.isInteger(windowMs) || windowMs <= 0) errors.push('Rate limit window must be a positive integer');
        lines.push(`rate_limit: ${limit}/${unit} window ${windowMs}ms,`);
    }
    if (entryConfig?.secureEnabled) {
        if (trimValue(entryConfig.secureRulesCode)) {
            lines.push(`secure: [${trimValue(entryConfig.secureRulesCode)}],`);
            return lines;
        }
        const schemes = trimValue(entryConfig.secureRules)
            .split(/[,\s]+/)
            .map((item) => item.trim())
            .filter(Boolean);
        if (!schemes.length) errors.push('Secure scheme is required');
        const checks = trimValue(entryConfig.secureChecks)
            .split('\n')
            .map((item) => item.trim())
            .filter(Boolean);
        const checkBlock = checks.length ? ` { checks: [${checks.join(', ')}] }` : '';
        lines.push(`secure: [${schemes.map((scheme) => `${scheme}${checkBlock}`).join(', ')}],`);
    }
    return lines;
}

export function buildEndpointOptionsSource(entryConfig, notify, silent = false) {
    const errors = [];
    const lines = generateEndpointOptions(entryConfig, errors);
    if (errors.length) {
        if (!silent) notify('error', errors[0]);
        return null;
    }
    return lines.map((line) => `    ${line}`).join('\n');
}

export function buildEndpointBody(nodes, targetEndpointIndex, notify, silent = false) {
    const errors = [];
    const sections = [];
    const entryNode = nodes.find((node) => node.data?.kind === 'entry' && node.data?.endpointIndex === targetEndpointIndex);
    const responseNode = nodes.find((node) => node.data?.kind === 'response' && node.data?.endpointIndex === targetEndpointIndex);
    const optionLines = generateEndpointOptions(entryNode?.data?.config, errors);
    if (optionLines.length) sections.push(optionLines.join('\n'));
    for (const node of orderedStepNodes(nodes, targetEndpointIndex)) {
        const code = generateNodeCode(node, errors);
        if (code) sections.push(code);
    }
    sections.push(generateResponseCode(responseNode, errors));
    if (errors.length) {
        if (!silent) notify('error', errors[0]);
        return '';
    }
    return sections.map((section) => section.split('\n').map((part) => `    ${part}`).join('\n')).join('\n\n');
}

function ensureValue(value, label, errors) {
    const trimmed = trimValue(value);
    if (!trimmed) errors.push(`${label} is required`);
    return trimmed;
}

function ensureIdentifier(value, label, errors) {
    const trimmed = ensureValue(value, label, errors);
    if (trimmed && !/^[A-Za-z_][A-Za-z0-9_]*$/.test(trimmed)) {
        errors.push(`${label} must be a valid identifier`);
    }
    return trimmed;
}

function finishStatement(code) {
    const trimmed = trimValue(code);
    if (!trimmed) return '';
    return /;\s*$/.test(trimmed) ? trimmed : `${trimmed};`;
}

function configBlock(config) {
    const lines = [];
    if (config.method && config.kind === 'fetch') lines.push(`method: ${JSON.stringify(config.method)}`);
    if (config.kind === 'fetch' && trimValue(config.body)) lines.push(`body: ${trimValue(config.body)}`);
    if (Number(config.timeoutMs) > 0) lines.push(`timeout: ${Number(config.timeoutMs)}ms`);
    if (Number(config.retries) > 0) lines.push(`retry: ${Number(config.retries)} times`);
    if (Number(config.delayMs) > 0) lines.push(`delay: ${Number(config.delayMs)}ms`);
    if (trimValue(config.fallback)) lines.push(`fallback: ${trimValue(config.fallback)}`);
    if (!lines.length) return '';
    return ` {\n${lines.map((line) => `        ${line}`).join(',\n')}\n    }`;
}

function generateNodeCode(node, errors) {
    const config = node.data.config;
    const rawCode = finishStatement(config.code);

    if (config.kind === 'fetch') {
        const variable = ensureIdentifier(config.variable, 'Fetch variable', errors);
        const url = ensureValue(config.url, 'Fetch URL expression', errors);
        if (!variable || !url) return '';
        return `fetch ${url} as ${variable}${configBlock(config)};`;
    }

    if (config.kind === 'pipe') {
        if (!rawCode) errors.push('Transform code is required');
        return rawCode;
    }

    if (config.kind === 'db') {
        const variable = ensureIdentifier(config.variable, 'DB variable', errors);
        const dbSource = ensureValue(config.dbSource, 'DB source', errors);
        const sql = ensureValue(config.sql, 'SQL', errors);
        if (!variable || !dbSource || !sql) return '';
        const params = trimValue(config.params);
        const args = [dbSource, JSON.stringify(sql), params].filter(Boolean).join(', ');
        return `let ${variable} = db::query(${args})${configBlock(config)};`;
    }

    if (config.kind === 'grpc') {
        const variable = ensureIdentifier(config.variable, 'gRPC variable', errors);
        const serviceMethod = ensureValue(config.serviceMethod, 'gRPC service method', errors);
        const payload = ensureValue(config.payload, 'gRPC payload', errors);
        if (!variable || !serviceMethod || !payload) return '';
        const protoPath = trimValue(config.protoPath);
        const service = trimValue(config.service);
        const method = trimValue(config.method);
        const args = protoPath || service || method
            ? [JSON.stringify(serviceMethod), JSON.stringify(protoPath), JSON.stringify(service), JSON.stringify(method), payload]
            : [JSON.stringify(serviceMethod), payload];
        return `let ${variable} = grpc::call(${args.join(', ')})${configBlock(config)};`;
    }

    if (config.kind === 'command') {
        const variable = ensureIdentifier(config.variable, 'Command variable', errors);
        const command = ensureValue(config.command, 'Command', errors);
        if (!variable || !command) return '';
        return `command ${JSON.stringify(command)} as ${variable};`;
    }

    if (config.kind === 'let') {
        const variable = ensureIdentifier(config.variable, 'Let variable', errors);
        const value = ensureValue(config.value, 'Let expression', errors);
        if (!variable || !value) return '';
        return `let ${variable} = ${value};`;
    }

    if (config.kind === 'step') {
        if (!rawCode) errors.push('Step code is required');
        return rawCode;
    }

    return '';
}

function generateResponseCode(responseNode, errors) {
    const config = responseNode?.data?.config ?? {status: 200, bodyCode: '{}'};
    const status = Number(config.status || 200);
    if (!Number.isInteger(status) || status < 100 || status > 599) {
        errors.push('Response status must be between 100 and 599');
    }
    const body = trimValue(config.bodyCode);
    const headers = trimValue(config.headersCode);
    const cookies = trimValue(config.cookiesCode);
    if (headers || cookies) {
        const parts = [];
        if (body) parts.push(`body ${normalizeObjectBlock(body)}`);
        if (headers) parts.push(`headers ${normalizeObjectBlock(headers)}`);
        if (cookies) parts.push(`cookies ${normalizeObjectBlock(cookies)}`);
        return `respond ${status} ${parts.join('\n')}`;
    }
    if (!body) return `respond ${status}`;
    return body.startsWith('{') ? `respond ${status} ${body}` : `respond ${status} {\n${body}\n}`;
}

function normalizeObjectBlock(value) {
    const trimmed = trimValue(value);
    return trimmed.startsWith('{') ? trimmed : `{\n${trimmed}\n}`;
}

function orderedStepNodes(nodes, targetEndpointIndex) {
    return nodes
        .filter((node) => node.data?.endpointIndex === targetEndpointIndex && !['entry', 'response'].includes(node.data?.kind))
        .slice()
        .sort((left, right) => {
            const dx = (left.position?.x ?? 0) - (right.position?.x ?? 0);
            if (Math.abs(dx) > 20) return dx;
            return (left.position?.y ?? 0) - (right.position?.y ?? 0);
        });
}

export function syncNodePresentation(node, changedField = null) {
    const config = node.data.config;
    node.data.kind = config.kind;
    if (config.kind === 'gateway') {
        node.data.title = `Gateway: ${config.name || 'gateway'}`;
        node.data.badge = 'root';
        node.data.handles = [{id: 'gateway', type: 'out', label: 'gateway'}];
    }
    if (config.kind === 'fetch') {
        node.data.title = `Fetch: ${config.url || 'url'}`;
        node.data.badge = config.variable || node.data.badge;
        if (changedField !== 'code') config.code = fetchConfigCode(config);
        node.data.handles = [{id: 'request', type: 'in', label: 'request'}, {
            id: config.variable || 'output',
            type: 'out',
            label: config.variable || 'output'
        }];
    }
    if (config.kind === 'pipe') {
        node.data.title = `Transform: ${config.variable || 'value'}`;
        node.data.badge = config.variable || node.data.badge;
        syncPipeConfig(config, changedField);
        node.data.handles = [{
            id: 'source',
            type: 'in',
            label: config.source || 'source'
        }, {id: config.variable || 'output', type: 'out', label: config.variable || 'output'}];
    }
    if (config.kind === 'response') {
        node.data.title = `Respond: ${config.status || 200}`;
    }
    if (config.kind === 'db') {
        node.data.title = `DB: ${config.variable || 'query'}`;
        node.data.badge = config.variable || node.data.badge;
        node.data.handles = [{id: 'params', type: 'in', label: 'params'}, {
            id: config.variable || 'rows',
            type: 'out',
            label: config.variable || 'rows'
        }];
    }
    if (config.kind === 'grpc') {
        node.data.title = `gRPC: ${config.variable || 'call'}`;
        node.data.badge = config.variable || node.data.badge;
        node.data.handles = [{id: 'payload', type: 'in', label: 'payload'}, {
            id: config.variable || 'result',
            type: 'out',
            label: config.variable || 'result'
        }];
    }
    if (config.kind === 'command') {
        node.data.title = `Command: ${config.variable || 'command'}`;
        node.data.badge = config.variable || node.data.badge;
        node.data.handles = [{id: 'deps', type: 'in', label: 'deps'}, {
            id: config.variable || 'output',
            type: 'out',
            label: config.variable || 'output'
        }];
    }
    if (config.kind === 'let') {
        node.data.title = `Let: ${config.variable || 'value'}`;
        node.data.badge = config.variable || node.data.badge;
        node.data.handles = [{id: 'deps', type: 'in', label: 'deps'}, {
            id: config.variable || 'output',
            type: 'out',
            label: config.variable || 'output'
        }];
    }
    if (config.kind === 'entry') {
        node.data.title = `${config.method || 'GET'} ${config.path || '/'}`;
        node.data.endpointIndex = config.endpointIndex;
    }
    node.data.lines = linesFromConfig(config);
}

function fetchConfigCode(config) {
    const lines = [
        `fetch ${trimValue(config.url) || 'url'} as ${trimValue(config.variable) || 'output'} {`,
        config.method ? `  method: ${JSON.stringify(config.method)},` : null,
        trimValue(config.body) ? `  body: ${trimValue(config.body)},` : null,
        Number(config.timeoutMs) > 0 ? `  timeout: ${Number(config.timeoutMs)}ms,` : null,
        Number(config.retries) > 0 ? `  retry: ${Number(config.retries)} times,` : null,
        Number(config.delayMs) > 0 ? `  delay: ${Number(config.delayMs)}ms,` : null,
        trimValue(config.fallback) ? `  fallback: ${trimValue(config.fallback)},` : null,
        '};'
    ];
    return lines.filter(Boolean).join('\n');
}

export function draftConfig(kind, index) {
    if (kind === 'fetch') {
        return {
            kind: 'fetch',
            variable: `fetch_${index}`,
            method: 'GET',
            url: '',
            timeoutMs: 500,
            retries: 0,
            delayMs: 0,
            fallback: '',
            code: ''
        };
    }
    if (kind === 'pipe') {
        return {kind: 'pipe', variable: `value_${index}`, source: '', code: ''};
    }
    if (kind === 'db') {
        return {
            kind: 'db',
            variable: `rows_${index}`,
            dbSource: '',
            sql: '',
            params: '',
            timeoutMs: 500,
            fallback: '',
            code: ''
        };
    }
    if (kind === 'grpc') {
        return {
            kind: 'grpc',
            variable: `grpc_${index}`,
            serviceMethod: '',
            protoPath: '',
            service: '',
            method: '',
            payload: '',
            timeoutMs: 500,
            fallback: '',
            code: ''
        };
    }
    if (kind === 'command') {
        return {kind: 'command', variable: `command_${index}`, command: '', code: ''};
    }
    if (kind === 'let') {
        return {kind: 'let', variable: `value_${index}`, value: '', code: ''};
    }
    return {kind: 'step', variable: `step_${index}`, code: ''};
}
