import type {
  EngineYield,
  JsonRpcError,
  JsonRpcResponse,
  RpcId,
} from './types'

export type EventListener = (y: EngineYield) => void
export type StatusListener = (status: ConnectionStatus) => void

export type ConnectionStatus =
  | 'connecting'
  | 'open'
  | 'closed'
  | 'error'

/**
 * OneAiRpcClient — a thin JSON-RPC 2.0 client over a raw WebSocket to the
 * `oneai app-server --listen ws://<host>:<port>` endpoint.
 *
 * Wire contract (verified against the Rust sources):
 *  - one WebSocket text frame = one JSON-RPC message, either direction;
 *  - inbound request: `{jsonrpc:"2.0", id, method, params}` → app-server
 *    replies with `{jsonrpc:"2.0", id, result|error}`;
 *  - outbound: only the `event` notification, params = the whole EngineYield
 *    (with its `kind` tag). Unknown `kind`s MUST be ignored.
 *
 * State management is intentionally NOT React-coupled: this is the external
 * store. React subscribes via `useSyncExternalStore` in `store/projection.ts`.
 */
export class OneAiRpcClient {
  private ws: WebSocket | null = null
  private url: string
  private nextId = 1
  private pending = new Map<RpcId, { resolve: (v: unknown) => void; reject: (e: Error) => void }>()
  private eventListeners = new Set<EventListener>()
  private statusListeners = new Set<StatusListener>()
  private status: ConnectionStatus = 'closed'
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private shouldReconnect = true

  constructor(url: string) {
    this.url = url
  }

  // ── lifecycle ─────────────────────────────────────────────────────────────

  connect(): void {
    if (this.ws !== null) return
    this.shouldReconnect = true
    this.setStatus('connecting')
    let ws: WebSocket
    try {
      ws = new WebSocket(this.url)
    } catch (e) {
      this.setStatus('error')
      this.scheduleReconnect()
      throw e
    }
    this.ws = ws

    ws.onopen = () => this.setStatus('open')
    ws.onmessage = (ev) => this.handleMessage(ev)
    ws.onerror = () => {
      // The browser fires `close` after `error`; just flip status here.
      this.setStatus('error')
    }
    ws.onclose = () => {
      this.ws = null
      this.setStatus('closed')
      // Reject any in-flight requests — the server won't answer them.
      for (const p of this.pending.values()) p.reject(new Error('connection closed'))
      this.pending.clear()
      if (this.shouldReconnect) this.scheduleReconnect()
    }
  }

  /** Stop reconnecting and tear down. Safe to call repeatedly. */
  dispose(): void {
    this.shouldReconnect = false
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
    if (this.ws !== null) {
      this.ws.onopen = this.ws.onmessage = this.ws.onerror = this.ws.onclose = null
      try {
        this.ws.close()
      } catch {
        /* ignore */
      }
      this.ws = null
    }
    this.setStatus('closed')
  }

  private scheduleReconnect(): void {
    if (!this.shouldReconnect) return
    if (this.reconnectTimer !== null) return
    // Simple fixed 1.5s backoff — keeps the dev loop responsive without
    // hammering a server that's intentionally down.
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, 1500)
  }

  // ── subscription ───────────────────────────────────────────────────────────

  onEvent(fn: EventListener): () => void {
    this.eventListeners.add(fn)
    return () => this.eventListeners.delete(fn)
  }
  onStatus(fn: StatusListener): () => void {
    this.statusListeners.add(fn)
    fn(this.status)
    return () => this.statusListeners.delete(fn)
  }
  getStatus(): ConnectionStatus {
    return this.status
  }
  private setStatus(s: ConnectionStatus): void {
    this.status = s
    for (const l of this.statusListeners) l(s)
  }

  // ── RPC ─────────────────────────────────────────────────────────────────────

  /** Call a JSON-RPC method and await its result. */
  call<P, R>(method: string, params: P): Promise<R> {
    if (this.ws === null || this.ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error(`not connected (${method})`))
    }
    const id: RpcId = this.nextId++
    const msg = JSON.stringify({ jsonrpc: '2.0', id, method, params })
    return new Promise<R>((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
      })
      try {
        this.ws!.send(msg)
      } catch (e) {
        this.pending.delete(id)
        reject(e as Error)
      }
    })
  }

  /** Fire-and-forget a notification (no id, no response). W1 doesn't emit any. */
  notify<P>(method: string, params: P): void {
    if (this.ws === null || this.ws.readyState !== WebSocket.OPEN) return
    this.ws.send(JSON.stringify({ jsonrpc: '2.0', method, params }))
  }

  // ── inbound ────────────────────────────────────────────────────────────────

  private handleMessage(ev: MessageEvent): void {
    let data: string
    if (typeof ev.data === 'string') data = ev.data
    else if (ev.data instanceof ArrayBuffer) data = new TextDecoder().decode(ev.data)
    else return
    let msg: JsonRpcResponse | { method: string; params: unknown }
    try {
      msg = JSON.parse(data)
    } catch {
      return // ignore malformed frames
    }

    // A response (has an id) — resolve the pending request.
    if ('id' in msg && msg.id !== undefined && msg.id !== null) {
      const id = msg.id as RpcId
      const p = this.pending.get(id)
      if (p === undefined) return
      this.pending.delete(id)
      const r = msg as JsonRpcResponse
      if (r.error !== undefined) {
        p.reject(toError(r.error))
      } else {
        p.resolve(r.result)
      }
      return
    }

    // A notification — only `event` is defined outbound.
    if ('method' in msg && msg.method === 'event') {
      const y = msg.params as EngineYield
      for (const l of this.eventListeners) {
        try {
          l(y)
        } catch {
          /* a bad listener must not kill the others */
        }
      }
    }
  }
}

function toError(e: JsonRpcError): Error {
  const err = new Error(e.message ?? `JSON-RPC error ${e.code}`)
  ;(err as Error & { code?: number }).code = e.code
  return err
}
