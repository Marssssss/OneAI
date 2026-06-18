/**
 * OneAI Studio — Graph Renderer (SVG + D3.js-style force graph)
 *
 * Renders StateGraph nodes and edges as an interactive SVG visualization.
 * No external D3.js dependency — pure SVG + JavaScript.
 */

// ─── Graph Layout Constants ──────────────────────────────────────────
const NODE_WIDTH = 120;
const NODE_HEIGHT = 50;
const NODE_PADDING = 30;
const EDGE_CURVE = 0.4;

// ─── Render Graph ────────────────────────────────────────────────────
function renderGraph(viz) {
    const svg = document.getElementById('graph-svg');
    const container = document.getElementById('graph-container');

    // Clear previous graph
    svg.innerHTML = '';

    const width = container.clientWidth;
    const height = container.clientHeight;

    svg.setAttribute('width', width);
    svg.setAttribute('height', height);

    if (!viz.nodes || viz.nodes.length === 0) {
        const text = createSvgElement('text', {
            x: width / 2, y: height / 2,
            fill: '#8892b0', 'font-size': '14', 'text-anchor': 'middle'
        });
        text.textContent = 'No graph data';
        svg.appendChild(text);
        return;
    }

    // Compute node positions using simple hierarchical layout
    const positions = computeLayout(viz, width, height);

    // Render edges first (below nodes)
    viz.edges.forEach(edge => {
        renderEdge(svg, viz, edge, positions);
    });

    // Render nodes
    viz.nodes.forEach(node => {
        renderNode(svg, viz, node, positions);
    });

    // Mark entry point
    const entryPos = positions[viz.entry_point];
    if (entryPos) {
        const marker = createSvgElement('circle', {
            cx: entryPos.x, cy: entryPos.y - NODE_HEIGHT / 2 - 8,
            r: 4, fill: '#4ecca3'
        });
        svg.appendChild(marker);
        const label = createSvgElement('text', {
            x: entryPos.x, y: entryPos.y - NODE_HEIGHT / 2 - 14,
            fill: '#4ecca3', 'font-size': '10', 'text-anchor': 'middle'
        });
        label.textContent = 'START';
        svg.appendChild(label);
    }

    // Mark terminals
    viz.terminals.forEach(term => {
        const pos = positions[term];
        if (pos) {
            const marker = createSvgElement('circle', {
                cx: pos.x, cy: pos.y + NODE_HEIGHT / 2 + 8,
                r: 4, fill: '#ff6b6b'
            });
            svg.appendChild(marker);
        }
    });
}

// ─── Layout Computation ──────────────────────────────────────────────
function computeLayout(viz, width, height) {
    const positions = {};

    // Build adjacency for depth calculation
    const depths = {};
    const visited = {};

    // BFS from entry point to assign depth levels
    const queue = [viz.entry_point];
    depths[viz.entry_point] = 0;
    visited[viz.entry_point] = true;

    while (queue.length > 0) {
        const current = queue.shift();
        const currentDepth = depths[current];

        viz.edges.forEach(edge => {
            if (edge.from === current && !visited[edge.to]) {
                depths[edge.to] = currentDepth + 1;
                visited[edge.to] = true;
                queue.push(edge.to);
            }
        });
    }

    // Assign unvisited nodes to remaining depths
    viz.nodes.forEach(node => {
        if (!visited[node.id]) {
            depths[node.id] = Object.values(depths).length;
        }
    });

    // Group nodes by depth level
    const levels = {};
    viz.nodes.forEach(node => {
        const d = depths[node.id] || 0;
        if (!levels[d]) levels[d] = [];
        levels[d].push(node.id);
    });

    // Calculate positions
    const maxDepth = Math.max(...Object.values(depths), 0);
    const levelHeight = height / (maxDepth + 2);

    Object.keys(levels).forEach(depth => {
        const nodesAtLevel = levels[depth];
        const levelWidth = width / (nodesAtLevel.length + 1);

        nodesAtLevel.forEach((nodeId, idx) => {
            positions[nodeId] = {
                x: levelWidth * (idx + 1),
                y: levelHeight * (parseInt(depth) + 1)
            };
        });
    });

    return positions;
}

// ─── Render Node ─────────────────────────────────────────────────────
function renderNode(svg, viz, node, positions) {
    const pos = positions[node.id];
    if (!pos) return;

    const isEntry = node.id === viz.entry_point;
    const isTerminal = viz.terminals.includes(node.id);
    const isInterrupt = node.interrupt;

    const group = createSvgElement('g', {
        class: `graph-node ${isInterrupt ? 'interrupt' : ''}`,
        transform: `translate(${pos.x - NODE_WIDTH / 2}, ${pos.y - NODE_HEIGHT / 2})`
    });

    // Node rectangle
    const rect = createSvgElement('rect', {
        width: NODE_WIDTH,
        height: NODE_HEIGHT,
        class: isTerminal ? 'terminal' : isEntry ? 'entry' : ''
    });

    // Node icon (based on action type)
    const icon = getActionIcon(node.action_type);

    // Node label
    const label = createSvgElement('text', {
        x: NODE_WIDTH / 2,
        y: NODE_HEIGHT / 2 + 4,
        'text-anchor': 'middle',
        'font-size': '11'
    });
    label.textContent = truncateLabel(node.label, 16);

    group.appendChild(rect);
    group.appendChild(label);

    // Action type badge
    const badge = createSvgElement('text', {
        x: NODE_WIDTH / 2,
        y: 10,
        'text-anchor': 'middle',
        'font-size': '10',
        fill: '#8892b0'
    });
    badge.textContent = icon;
    group.appendChild(badge);

    svg.appendChild(group);
}

// ─── Render Edge ─────────────────────────────────────────────────────
function renderEdge(svg, viz, edge, positions) {
    const fromPos = positions[edge.from];
    const toPos = positions[edge.to];
    if (!fromPos || !toPos) return;

    const isConditional = !edge.is_unconditional;

    const group = createSvgElement('g', {
        class: `graph-edge ${isConditional ? 'conditional' : ''}`
    });

    // Calculate path
    const startX = fromPos.x;
    const startY = fromPos.y + NODE_HEIGHT / 2;
    const endX = toPos.x;
    const endY = toPos.y - NODE_HEIGHT / 2;

    // If same position (self-loop), draw a loop
    if (edge.from === edge.to) {
        const loopPath = createSvgElement('path', {
            d: `M ${startX} ${startY}
                C ${startX - 40} ${startY + 30},
                  ${startX - 40} ${startY - 30},
                  ${startX} ${startY - NODE_HEIGHT / 2}`,
            class: 'edge-path'
        });
        group.appendChild(loopPath);
    } else {
        // Bezier curve for edges
        const midY = (startY + endY) / 2;
        const path = createSvgElement('path', {
            d: `M ${startX} ${startY}
                C ${startX} ${midY},
                  ${endX} ${midY},
                  ${endX} ${endY}`,
            class: 'edge-path'
        });
        group.appendChild(path);

        // Arrow at end
        const arrow = createSvgElement('polygon', {
            points: `${endX - 4},${endY - 6} ${endX},${endY} ${endX + 4},${endY - 6}`,
            fill: isConditional ? '#4a90d9' : '#8892b0'
        });
        group.appendChild(arrow);
    }

    // Edge condition label
    if (edge.label) {
        const labelX = (startX + endX) / 2;
        const labelY = (startY + endY) / 2 - 10;

        const label = createSvgElement('text', {
            x: labelX, y: labelY,
            'text-anchor': 'middle',
            'font-size': '10',
            fill: isConditional ? '#4a90d9' : '#8892b0'
        });
        label.textContent = edge.label;
        group.appendChild(label);
    }

    svg.appendChild(group);
}

// ─── Helpers ─────────────────────────────────────────────────────────
function createSvgElement(tag, attrs) {
    const el = document.createElementNS('http://www.w3.org/2000/svg', tag);
    Object.entries(attrs).forEach(([key, value]) => {
        el.setAttribute(key, value);
    });
    return el;
}

function getActionIcon(actionType) {
    const icons = {
        'llm_infer': '🧠',
        'tool_call': '🔧',
        'delegate': '🤖',
        'human_approval': '✋',
        'condition_check': '🔀',
        'switch_paradigm': '🔄',
    };
    return icons[actionType] || '⬜';
}

function truncateLabel(str, maxLen) {
    if (!str) return '';
    return str.length > maxLen ? str.substring(0, maxLen) : str;
}
