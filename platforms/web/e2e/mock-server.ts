// Mock app-server over raw WebSocket for Playwright e2e. Speaks the same
// JSON-RPC 2.0 protocol the real `oneai app-server --listen ws://` does, but
// replays scripted EngineYield sequences (see ./fixtures/scripts.ts) instead
// of running an agent. Deterministic + Rust-free.
//
// Wired up by `e2e/globalSetup.ts` (started on :8788) while Playwright's
// `webServer` runs `vite` with VITE_APP_SERVER_URL pointing here.
import { WebSocketServer, type WebSocket } from 'ws'
import type { EngineYield } from '../src/rpc/types'
import { singleAgentReply, groupRound, approvalPre, approvalPost } from './fixtures/scripts'

const PORT = Number(process.env.ONEAI_E2E_WS_PORT ?? 8788)
const TURN = 'turn-e2e'
const TICK = 25 // ms between scripted events — slow enough to exercise streaming

/** A connection's in-flight state. */
interface ConnState {
  /** True while a scripted turn has pushed an approval_request and is paused
   * until the client sends approval/respond. */
  awaitingApproval: boolean
}

function send(ws: WebSocket, obj: unknown): void {
  if (ws.readyState === ws.OPEN) ws.send(JSON.stringify(obj))
}
function respond(ws: WebSocket, id: number | string | undefined, result: unknown): void {
  send(ws, { jsonrpc: '2.0', id, result })
}
/** Push `event` notifications (EngineYield) with small delays. */
function replay(ws: WebSocket, events: EngineYield[], delay = TICK): void {
  events.forEach((e, i) => {
    setTimeout(() => send(ws, { jsonrpc: '2.0', method: 'event', params: e }), (i + 1) * delay)
  })
}

export async function startMockServer(): Promise<{ close: () => Promise<void> }> {
  const wss = new WebSocketServer({ port: PORT })
  await new Promise<void>((resolve) => wss.on('listening', () => resolve()))

  wss.on('connection', (ws) => {
    const st: ConnState = { awaitingApproval: false }

    ws.on('message', (raw) => {
      let msg: { id?: number | string; method?: string; params?: unknown }
      try {
        msg = JSON.parse(raw.toString())
      } catch {
        return
      }
      const { id, method, params } = msg
      if (method === undefined) return

      switch (method) {
        // ── session ──
        case 'session/create':
          respond(ws, id, { id: 'sess-e2e' })
          break
        case 'session/list':
          respond(ws, id, { sessions: [] })
          break
        case 'session/load':
          respond(ws, id, { id: (params as { id?: string })?.id ?? 'sess-e2e' })
          break
        case 'session/clear':
        case 'session/delete':
          respond(ws, id, { ok: true })
          break

        // ── scenario / domainpack / skills / config / provider ──
        case 'scenario/list':
          respond(ws, id, { scenarios: [] })
          break
        case 'scenario/validate':
        case 'scenario/upsert':
        case 'scenario/delete':
          respond(ws, id, { ok: true, errors: [] })
          break
        case 'domainpack/list':
          respond(ws, id, { packs: [] })
          break
        case 'skill/list':
          respond(ws, id, { skills: [] })
          break
        case 'config/get':
          respond(ws, id, { active_provider: null, providers: [] })
          break
        case 'config/read':
          respond(ws, id, { path: '~/.oneai/config.toml', content: '' })
          break
        case 'config/update':
          respond(ws, id, { ok: true })
          break
        case 'provider/list':
          respond(ws, id, { providers: [] })
          break
        case 'provider/add':
        case 'provider/delete':
        case 'provider/set_active':
          respond(ws, id, { ok: true })
          break
        case 'feedback/submit':
          respond(ws, id, { ok: true })
          break
        case 'group/set_order':
          respond(ws, id, { ok: true })
          break

        // ── turns ──
        case 'turn/run': {
          respond(ws, id, { turn_id: TURN })
          // A turn whose user message contains "approve" triggers a tool call
          // that pauses for approval; otherwise stream a plain canned reply.
          const content = (params as { content?: { type: string; text?: string }[] })
            ?.content
          const text = (content ?? [])
            .filter((b) => b.type === 'text')
            .map((b) => b.text ?? '')
            .join(' ')
          if (text.toLowerCase().includes('approve')) {
            st.awaitingApproval = true
            replay(ws, approvalPre(TURN))
          } else {
            replay(ws, singleAgentReply('Hello from the OneAI mock server!', TURN))
          }
          break
        }
        case 'turn/cancel':
          respond(ws, id, { ok: true })
          break

        // ── group chat ──
        case 'group/start':
        case 'group/open':
          respond(ws, id, { ok: true })
          replay(
            ws,
            groupRound([
              { id: 'interviewer', text: 'Tell me about yourself.' },
              { id: 'coach', text: 'Good start — be specific.' },
            ]),
          )
          break
        case 'group/run':
          respond(ws, id, { ok: true })
          replay(
            ws,
            groupRound([
              { id: 'interviewer', text: 'Next question.' },
              { id: 'coach', text: 'Sharpen the answer.' },
            ]),
          )
          break

        // ── approval ──
        case 'approval/respond':
          if (st.awaitingApproval) {
            st.awaitingApproval = false
            respond(ws, id, { ok: true })
            replay(ws, approvalPost(TURN))
          } else {
            respond(ws, id, { ok: true })
          }
          break

        default:
          respond(ws, id, {
            error: { code: -32601, message: `method not found: ${method}` },
          })
      }
    })
  })

  return {
    close: () =>
      new Promise<void>((resolve) => {
        wss.close(() => resolve())
      }),
  }
}
