export function normalizeVisibleEndpoints(current, count, {initialized = false, includeLast = false} = {}) {
    if (!count) return [];
    if (!initialized) return allEndpointIndexes(count);

    const visible = current.filter((index) => index >= 0 && index < count);
    if (includeLast) visible.push(count - 1);
    return uniqueSorted(visible);
}

export function allEndpointIndexes(count) {
    return Array.from({length: count}, (_, index) => index);
}

export function toggleEndpointIndex(current, index) {
    return setEndpointIndexVisible(current, index, !current.includes(index));
}

export function setEndpointIndexVisible(current, index, visible) {
    const next = visible
        ? [...current, index]
        : current.filter((item) => item !== index);
    return uniqueSorted(next);
}

function uniqueSorted(items) {
    return [...new Set(items)].sort((left, right) => left - right);
}
