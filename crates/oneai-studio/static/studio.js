/**
 * OneAI Studio — Main JavaScript (WebSocket, DOM updates, API calls)
 */

// ─── State ──────────────────────────────────────────────────────────
let ws = null;
let totalTokens = 0;
let totalCost = 0;
let toolCallCount = 0;
let errorCount = 0;

// ─── WebSocket Connection ────────────────────────────────────────────
function connectWebSocket() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${protocol}//${location.host}/ws`;

    ws = new WebSocket(url);

    ws.onopen = () => {
        updateConnectionStatus(true);
        addTraceEntry('system', 'WebSocket connected');
    };

    ws.onmessage = (event) => {
        const data = JSON.parse(event.data);
        handleStudioEvent(data);
    };

    ws.onclose = () => {
        updateConnectionStatus(false);
        addTraceEntry('system', 'WebSocket disconnected — reconnecting...');
        setTimeout(connectWebSocket, 3000);
    };

    ws.onerror = () => {
        updateConnectionStatus(false);
    };
}

// ─── Event Handler ───────────────────────────────────────────────────
function handleStudioEvent(event) {
    const type = event.type || Object.keys(event)[0];

    // Flatten the event — it may be { "IterationStart": { ... } } or { "type": "IterationStart", ... }
    let kind, payload;
    if (event.type) {
        kind = event.type;
        payload = event;
    } else {
        // Serde serialized enum: { "VariantName": { fields... } }
        kind = Object.keys(event)[0];
        payload = event[kind];
    }

    switch (kind) {
        case 'IterationStart':
            addTraceEntry('iteration', `Iteration ${payload.iteration} — ${payload.paradigm}`);
            updateParadigmBadge(payload.paradigm);
            updateIterationBadge(payload.iteration);
            break;

        case 'DirectAnswer':
            addTraceEntry('answer', `Direct answer: ${truncate(payload.text, 100)}`);
            addOutputEntry('direct-answer', 'Direct Answer', payload.text);
            break;

        case 'ToolCalls':
            addTraceEntry('tool-call', `Tool calls: ${payload.calls.map(c => c.tool_name).join(', ')}`);
            payload.calls.forEach(c => {
                addOutputEntry('tool-output', `🔧 ${c.tool_name}`, JSON.stringify(c.args, null, 2));
            });
            toolCallCount += payload.calls.length;
            updateMetrics();
            break;

        case 'ToolResult':
            const successIcon = payload.success ? '✅' : '❌';
            addTraceEntry(payload.success ? 'tool-result' : 'error',
                `${successIcon} ${payload.tool_name}: ${truncate(payload.output_summary, 80)}`);
            addOutputEntry(payload.success ? 'tool-output' : 'error',
                payload.success ? `Result: ${payload.tool_name}` : `Error: ${payload.tool_name}`,
                payload.output_summary);
            break;

        case 'Delegate':
            addTraceEntry('iteration', `Delegating to ${payload.agent_type}: ${truncate(payload.task, 60)}`);
            break;

        case 'ParadigmSwitch':
            addTraceEntry('iteration', `Switching paradigm → ${payload.paradigm}`);
            updateParadigmBadge(payload.paradigm);
            break;

        case 'CheckpointSaved':
            addTraceEntry('iteration', `Checkpoint saved: iter ${payload.iteration}`);
            break;

        case 'TraceEvent':
            if (payload.kind === 'TokenUsage') {
                totalTokens += (payload.attributes?.total_tokens || 0);
                updateMetrics();
            } else if (payload.kind === 'CostUpdate') {
                totalCost = payload.attributes?.estimated_cost_usd || 0;
                updateMetrics();
            }
            addTraceEntry('trace', `${payload.kind}: ${payload.name}`);
            break;

        case 'Thinking':
            addTraceEntry('thinking', `💭 ${truncate(payload.text, 80)}`);
            break;

        case 'StreamChunk':
            addTraceEntry('trace', `Chunk: ${truncate(payload.text, 40)}`);
            break;

        case 'ApprovalRequest':
            addTraceEntry('approval', `⚠️ Approval needed: ${payload.tool_name} (${payload.risk_level})`);
            break;

        case 'ApprovalResponse':
            addTraceEntry(payload.approved ? 'iteration' : 'error',
                `Approval: ${payload.approved ? '✅ Approved' : '❌ Denied'} — ${payload.reason}`);
            break;

        case 'LoopComplete':
            addTraceEntry('iteration', `✅ Loop complete: ${payload.result_summary}`);
            break;

        case 'Error':
            addTraceEntry('error', `❌ Error: ${payload.message}`);
            errorCount++;
            updateMetrics();
            break;

        case 'connected':
            addTraceEntry('system', payload.message || 'Connected');
            break;

        default:
            addTraceEntry('trace', `${kind}: ${JSON.stringify(payload).substring(0, 60)}`);
    }
}

// ─── DOM Updates ─────────────────────────────────────────────────────
function updateConnectionStatus(connected) {
    const dot = document.getElementById('ws-dot');
    const text = document.getElementById('ws-status-text');
    if (connected) {
        dot.className = 'status-dot connected';
        text.textContent = 'Connected';
    } else {
        dot.className = 'status-dot disconnected';
        text.textContent = 'Disconnected';
    }
}

function updateParadigmBadge(paradigm) {
    document.getElementById('paradigm-badge').textContent = paradigm.toUpperCase();
}

function updateIterationBadge(iteration) {
    document.getElementById('iteration-badge').textContent = `Iter: ${iteration}`;
}

function updateMetrics() {
    document.getElementById('metric-tokens').textContent = totalTokens > 1000 ?
        `${(totalTokens / 1000).toFixed(1)}K` : totalTokens;
    document.getElementById('metric-cost').textContent = `$${totalCost.toFixed(4)}`;
    document.getElementById('metric-tools').textContent = toolCallCount;
    document.getElementById('metric-errors').textContent = errorCount;
}

function addTraceEntry(type, text) {
    const list = document.getElementById('trace-list');
    const empty = list.querySelector('.trace-empty');
    if (empty) empty.remove();

    const entry = document.createElement('div');
    entry.className = `trace-entry ${type}`;
    entry.innerHTML = `<span class="event-kind">${type}</span> ${text}`;
    list.appendChild(entry);
    list.scrollTop = list.scrollHeight;
}

function addOutputEntry(type, label, content) {
    const container = document.getElementById('output-content');
    const empty = container.querySelector('.output-empty');
    if (empty) empty.remove();

    const entry = document.createElement('div');
    entry.className = `output-entry ${type}`;
    entry.innerHTML = `<div class="label">${label}</div><div>${content}</div>`;
    container.appendChild(entry);
    container.scrollTop = container.scrollHeight;
}

function truncate(str, maxLen) {
    if (!str) return '';
    return str.length > maxLen ? str.substring(0, maxLen) + '...' : str;
}

// ─── API Calls ───────────────────────────────────────────────────────
async function loadGraphs() {
    const select = document.getElementById('graph-select');
    const response = await fetch('/api/graph');
    const data = await response.json();

    data.graphs.forEach(name => {
        const option = document.createElement('option');
        option.value = name;
        option.textContent = name;
        select.appendChild(option);
    });
}

async function loadAndRenderGraph(name) {
    if (!name) return;
    const response = await fetch(`/api/graph/${name}`);
    if (!response.ok) return;
    const viz = await response.json();
    renderGraph(viz);
}

async function loadCheckpoints() {
    const response = await fetch('/api/checkpoint');
    const data = await response.json();

    const list = document.getElementById('checkpoint-list');
    list.innerHTML = '';

    if (data.checkpoints && data.checkpoints.length > 0) {
        data.checkpoints.forEach(cp => {
            const entry = document.createElement('div');
            entry.className = 'checkpoint-entry';
            entry.innerHTML = `<span class="cp-id">${cp.id}</span>
                <span class="cp-time">${cp.timestamp}</span>
                <span>${cp.paradigm} — step ${cp.step || '?'}</span>`;
            list.appendChild(entry);
        });
    } else {
        list.innerHTML = '<div style="color: var(--text-muted); text-align: center; padding: 8px;">No checkpoints found</div>';
    }
}

// ─── Initialize ──────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', () => {
    connectWebSocket();
    loadGraphs();

    document.getElementById('graph-select').addEventListener('change', (e) => {
        loadAndRenderGraph(e.target.value);
    });

    document.getElementById('load-checkpoints-btn').addEventListener('click', loadCheckpoints);
});
