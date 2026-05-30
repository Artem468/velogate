import {formatExpr, trimValue} from './expressions.js';

export function pipeCode(step) {
    const ops = step.operations.map((op) => `| ${pipeOpCode(op)}`).join('\n');
    return `let ${step.var_name} = ${formatExpr(step.source)}\n${ops};`;
}

export function syncPipeConfig(config, changedField = null) {
    if (changedField === 'code') {
        const header = parsePipeHeader(config.code);
        if (header) {
            config.variable = header.variable;
            config.source = header.source;
        }
        return;
    }

    if (changedField === 'variable' || changedField === 'source') {
        config.code = updatePipeHeader(config);
    }
}

function pipeOpCode(op) {
    switch (op.kind) {
        case 'closure':
            return `${op.name}(${op.param} => ${formatPipeExpr(op.value)})`;
        case 'expr':
            return `${op.name}(${formatPipeExpr(op.value)})`;
        case 'reduce':
            return `${op.name}(${formatPipeExpr(op.initial)}, ${op.acc}, ${op.param} => ${formatPipeExpr(op.value)})`;
        case 'none':
            return `${op.name}()`;
        default:
            return op.kind;
    }
}

function formatPipeExpr(value, indent = 0) {
    if (value?.kind === 'object') {
        const pad = ' '.repeat(indent);
        const innerPad = ' '.repeat(indent + 4);
        const entries = Object.entries(value.fields);
        if (!entries.length) return '{}';
        return `{\n${entries.map(([key, item]) => `${innerPad}${JSON.stringify(key)}: ${formatPipeExpr(item, indent + 4)}`).join(',\n')}\n${pad}}`;
    }
    if (value?.kind === 'array') {
        return `[${value.items.map((item) => formatPipeExpr(item, indent)).join(', ')}]`;
    }
    return formatExpr(value);
}

function parsePipeHeader(code) {
    const match = trimValue(code).match(/^let\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([^\n|;]+)/);
    if (!match) return null;
    return {
        variable: match[1],
        source: match[2].trim()
    };
}

function updatePipeHeader(config) {
    const variable = trimValue(config.variable) || 'value';
    const source = trimValue(config.source) || 'source';
    const nextHeader = `let ${variable} = ${source}`;
    const current = trimValue(config.code);
    if (!current) return `${nextHeader}\n| map(item => item);`;

    const lines = current.split('\n');
    if (/^let\s+[A-Za-z_][A-Za-z0-9_]*\s*=/.test(lines[0].trim())) {
        lines[0] = nextHeader;
        return lines.join('\n');
    }

    return `${nextHeader}\n${current}`;
}
