/**
 * OneAI Studio — Main JavaScript (chat, WebSocket, streaming, API calls)
 */

// ─── State ──────────────────────────────────────────────────────────
let ws = null;
let totalTokens = 0;
let toolCallCount = 0;
let errorCount = 0;
let iterCount = 0;
let runsSucceeded = 0;
let runsAttempted = 0;

/** The currently-streaming assistant message element (created on first
 *  StreamChunk / Thinking of a turn, finalized on DirectAnswer /
 *  LoopComplete). null when no turn is in flight. */
let currentAssistant = null;
let busy = false;

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
        let data;
        try { data = JSON.parse(event.data); } catch (e) { return; }
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
    // The welcome message is { "type": "connected", ... }; serde-tagged
    // agent events are { "VariantName": { ... } }. Normalize both.
    let kind, payload;
    if (event.type) {
        kind = event.type;
        payload = event;
    } else {
        kind = Object.keys(event)[0];
        payload = event[kind];
    }

    switch (kind) {
        case 'IterationStart':
            iterCount = payload.iteration;
            addTraceEntry('iteration', `Iteration ${payload.iteration} — ${payload.paradigm}`);
            updateParadigmBadge(payload.paradigm);
            updateIterationBadge(payload.iteration);
            updateMetrics();
            // Open a "thinking" bubble so the user sees activity before the
            // first stream chunk lands.
            ensureAssistantBubble('thinking');
            break;

        case 'DirectAnswer':
            addTraceEntry('answer', `Direct answer: ${truncate(payload.text, 100)}`);
            finalizeAssistant(payload.text);
            break;

        case 'ToolCalls':
            addTraceEntry('tool-call', `Tool calls: ${payload.calls.map(c => c.tool_name).join(', ')}`);
            payload.calls.forEach(c => {
                addTraceEntry('tool-call', `  ↳ ${c.tool_name}: ${truncate(JSON.stringify(c.args), 80)}`);
            });
            toolCallCount += payload.calls.length;
            updateMetrics();
            break;

        case 'ToolResult':
            addTraceEntry(payload.success ? 'tool-result' : 'error',
                `${payload.success ? '✅' : '❌'} ${payload.tool_name}: ${truncate(payload.output_summary, 80)}`);
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
            }
            addTraceEntry('trace', `${payload.kind}: ${payload.name}`);
            break;

        case 'Thinking':
            ensureAssistantBubble('thinking');
            appendToAssistant(payload.text, true);
            addTraceEntry('thinking', `💭 ${truncate(payload.text, 80)}`);
            break;

        case 'StreamChunk':
            // Accumulate into the live assistant bubble (typewriter).
            ensureAssistantBubble('assistant');
            appendToAssistant(payload.text, false);
            addTraceEntry('trace', `Chunk: ${truncate(payload.text, 40)}`);
            break;

        case 'LoopComplete':
            addTraceEntry('iteration', `✅ Loop complete: ${payload.result_summary}`);
            finalizeAssistant(null);
            setBusy(false);
            runsSucceeded++;
            updateMetrics();
            break;

        case 'Error':
            addTraceEntry('error', `❌ Error: ${payload.message}`);
            addMessage('assistant', `⚠️ Error: ${payload.message}`, 'error');
            finalizeAssistant(null);
            setBusy(false);
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

// ─── Assistant bubble lifecycle ───────────────────────────────────────
/** Create a streaming assistant bubble if none is active for this turn. */
function ensureAssistantBubble(kind) {
    if (currentAssistant) return;
    currentAssistant = addMessage(kind === 'thinking' ? 'assistant thinking' : 'assistant', '');
    if (kind === 'thinking') {
        currentAssistant.textContent = '';
        const tag = document.createElement('span');
        tag.className = 'role-tag';
        tag.textContent = 'thinking…';
        currentAssistant.appendChild(tag);
    }
    currentAssistant.classList.add('cursor');
    scrollOutput();
}

/** Append text to the active bubble. `isThinking` routes to a muted sub-line. */
function appendToAssistant(text, isThinking) {
    if (!currentAssistant || !text) return;
    if (isThinking) {
        let t = currentAssistant.querySelector('.thinking-text');
        if (!t) {
            t = document.createElement('span');
            t.className = 'thinking-text';
            t.style.color = 'var(--text-muted)';
            t.style.display = 'block';
            currentAssistant.appendChild(t);
        }
        t.textContent += text;
    } else {
        let t = currentAssistant.querySelector('.answer-text');
        if (!t) {
            currentAssistant.classList.remove('cursor');
            // drop a leftover thinking tag if streaming answer starts
            currentAssistant.textContent = '';
            t = document.createElement('span');
            t.className = 'answer-text';
            currentAssistant.appendChild(t);
        }
        t.textContent += text;
    }
    scrollOutput();
}

/** Finalize the active bubble (optionally set the full answer text). */
function finalizeAssistant(fullText) {
    if (!currentAssistant) {
        if (fullText) addMessage('assistant', fullText);
        return;
    }
    currentAssistant.classList.remove('cursor');
    if (fullText != null) {
        currentAssistant.textContent = '';
        const t = document.createElement('span');
        t.className = 'answer-text';
        t.textContent = fullText;
        currentAssistant.appendChild(t);
    }
    currentAssistant = null;
    scrollOutput();
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
    document.getElementById('paradigm-badge').textContent = (paradigm || '—').toUpperCase();
}

function updateIterationBadge(iteration) {
    document.getElementById('iteration-badge').textContent = `Iter: ${iteration}`;
}

function updateMetrics() {
    document.getElementById('metric-tokens').textContent = totalTokens > 1000
        ? `${(totalTokens / 1000).toFixed(1)}K` : totalTokens;
    document.getElementById('metric-iters').textContent = iterCount;
    const rate = runsAttempted > 0 ? Math.round(100 * runsSucceeded / runsAttempted) : 0;
    document.getElementById('metric-success').textContent = `${rate}%`;
    document.getElementById('metric-tools').textContent = toolCallCount;
    document.getElementById('metric-errors').textContent = errorCount;
}

function addTraceEntry(type, text) {
    const list = document.getElementById('trace-list');
    const empty = list.querySelector('.trace-empty');
    if (empty) empty.remove();

    const entry = document.createElement('div');
    entry.className = `trace-entry ${type}`;
    entry.innerHTML = `<span class="event-kind">${type}</span> ${escapeHtml(text)}`;
    list.appendChild(entry);
    list.scrollTop = list.scrollHeight;
}

function addMessage(role, text, extraClass) {
    const container = document.getElementById('output-content');
    const empty = document.getElementById('output-empty');
    if (empty) empty.remove();

    const entry = document.createElement('div');
    entry.className = `msg ${role}${extraClass ? ' ' + extraClass : ''}`;
    if (role === 'assistant' || role === 'assistant thinking') {
        const tag = document.createElement('span');
        tag.className = 'role-tag';
        tag.textContent = 'agent';
        entry.appendChild(tag);
        const body = document.createElement('span');
        body.className = 'answer-text';
        body.textContent = text;
        entry.appendChild(body);
    } else {
        const tag = document.createElement('span');
        tag.className = 'role-tag';
        tag.textContent = role;
        entry.appendChild(tag);
        const body = document.createElement('span');
        entry.appendChild(body);
        body.textContent = text;
    }
    container.appendChild(entry);
    scrollOutput();
    return entry;
}

function scrollOutput() {
    const c = document.getElementById('output-content');
    c.scrollTop = c.scrollHeight;
}

function truncate(str, maxLen) {
    if (!str) return '';
    return str.length > maxLen ? str.substring(0, maxLen) + '...' : str;
}

function escapeHtml(s) {
    return String(s)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// ─── Send a prompt ────────────────────────────────────────────────────
async function sendPrompt(prompt) {
    prompt = (prompt || '').trim();
    if (!prompt || busy) return;

    runsAttempted++;
    setBusy(true);
    // Show the user's message immediately.
    addMessage('user', prompt);
    currentAssistant = null; // a new run starts a fresh assistant bubble

    try {
        const res = await fetch('/api/run', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ prompt }),
        });
        if (res.status === 202) return; // events will stream over WS
        let msg = `Request rejected (HTTP ${res.status})`;
        try { const j = await res.json(); if (j.error) msg = j.error; } catch (_) {}
        addMessage('system', msg);
        setBusy(false);
    } catch (e) {
        addMessage('system', `Network error: ${e.message}`);
        setBusy(false);
    }
}

function setBusy(b) {
    busy = b;
    const btn = document.getElementById('chat-send');
    btn.disabled = b;
    btn.textContent = b ? 'Running…' : 'Send';
}

// ─── API Calls ───────────────────────────────────────────────────────
async function loadGraphs() {
    const select = document.getElementById('graph-select');
    try {
        const response = await fetch('/api/graph');
        const data = await response.json();
        (data.graphs || []).forEach(name => {
            const option = document.createElement('option');
            option.value = name;
            option.textContent = name;
            select.appendChild(option);
        });
    } catch (e) { /* ignore — graph panel is non-critical */ }
}

async function loadAndRenderGraph(name) {
    if (!name) return;
    const response = await fetch(`/api/graph/${name}`);
    if (!response.ok) return;
    const viz = await response.json();
    if (typeof renderGraph === 'function') renderGraph(viz);
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
            entry.innerHTML = `<span class="cp-id">${escapeHtml(cp.id)}</span>
                <span class="cp-time">${escapeHtml(cp.timestamp || '')}</span>
                <span>${escapeHtml(cp.paradigm || '')} — step ${cp.step || '?'}</span>`;
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

    const input = document.getElementById('chat-input');
    const form = document.getElementById('chat-form');

    form.addEventListener('submit', (e) => {
        e.preventDefault();
        const text = input.value;
        if (!text.trim() || busy) return;
        input.value = '';
        autoSize(input);
        sendPrompt(text);
    });

    // Enter to send, Shift+Enter for newline.
    input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            form.requestSubmit();
        }
    });
    input.addEventListener('input', () => autoSize(input));

    document.getElementById('graph-select').addEventListener('change', (e) => {
        loadAndRenderGraph(e.target.value);
    });
    document.getElementById('load-checkpoints-btn').addEventListener('click', loadCheckpoints);
});

function autoSize(el) {
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 120) + 'px';
}
