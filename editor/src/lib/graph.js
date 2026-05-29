export function graphSnapshot(nodes, selected, selectedEdge) {
    return {
        selectedId: selected?.id ?? null,
        selectedEdgeId: selectedEdge?.id ?? null,
        positions: new Map(nodes.map((node) => [
            node.id,
            {
                x: Math.round(node.position?.x ?? 0),
                y: Math.round(node.position?.y ?? 0)
            }
        ]))
    };
}

export function normalizeGraphPositions(items) {
    if (!items.length) return items;
    const targetX = 40;
    const targetY = 28;
    const minX = Math.min(...items.map((node) => node.position?.x ?? 0));
    const minY = Math.min(...items.map((node) => node.position?.y ?? 0));
    const offsetX = minX < targetX ? targetX - minX : -minX + targetX;
    const offsetY = minY < targetY ? targetY - minY : -minY + targetY;
    return items.map((node) => ({
        ...node,
        position: {
            x: Math.round((node.position?.x ?? 0) + offsetX),
            y: Math.round((node.position?.y ?? 0) + offsetY)
        }
    }));
}
