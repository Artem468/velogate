export function trimValue(value) {
    return String(value ?? '').trim();
}

export function formatExpr(value) {
    if (!value) return '';
    switch (value.kind) {
        case 'null':
            return 'null';
        case 'variable':
            return value.name;
        case 'number':
            return String(value.value);
        case 'string':
            return JSON.stringify(value.value);
        case 'boolean':
            return String(value.value);
        case 'property_access':
            return `${formatExpr(value.object)}.${value.field}`;
        case 'binary_op':
            return `${formatExpr(value.left)} ${value.op} ${formatExpr(value.right)}`;
        case 'call':
            return `${formatExpr(value.callee)}(${value.args.map(formatExpr).join(', ')})`;
        case 'array':
            return `[${value.items.map(formatExpr).join(', ')}]`;
        case 'object':
            return `{ ${Object.entries(value.fields).map(([key, item]) => `${JSON.stringify(key)}: ${formatExpr(item)}`).join(', ')} }`;
        default:
            return value.kind;
    }
}
