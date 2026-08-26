// TrajectoryExplorer — the resident full-area trajectory view (issue #40).
//
// A horizontal swim-lane timeline: lane 0 = main agent, one lane per delegated
// sub-agent; nodes are color-coded markers positioned by wall-clock time;
// fork/join/depends edges draw the delegation DAG. Drag to pan, wheel to zoom,
// click a node for its type-specific detail below. Defaults to a fit-to-width
// scale so the whole process is visible in one screen; "回到最新" jumps to the
// latest node.

import { useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { ProjectionSnapshot } from '../store/projection'
import { buildTimeline } from './timeline'
import { colorFor, LEGEND } from './colors'
import { DetailPane } from './DetailPane'
import styles from './TrajectoryExplorer.module.css'

interface TrajectoryExplorerProps {
  snapshot: ProjectionSnapshot
}

const LANE_H = 22
const LANE_GAP = 6
const NODE_R = 10
/** Pixels per node index at scale 1 — the uniform spacing the timeline uses
 *  instead of wall-clock gaps (issue #40 follow-up: raw elapsed time between
 *  iterations scatters nodes too sparsely). */
const NODE_GAP = 40
/** Min scale (px/node = NODE_GAP × scale) — collapses a long session tight. */
const MIN_SCALE = 0.05
const MAX_SCALE = 4
/** Fit-to-width caps the zoom-in so a short session doesn't over-spread. */
const MAX_FIT_SCALE = 3
/** Horizontal padding (px) so the fitted timeline doesn't touch the edges. */
const FIT_PAD = 56

export function TrajectoryExplorer({ snapshot }: TrajectoryExplorerProps): ReactNode {
  const { t } = useLocale()
  const model = useMemo(() => buildTimeline(snapshot.trajectory), [snapshot.trajectory])

  const canvasRef = useRef<HTMLDivElement | null>(null)
  const [width, setWidth] = useState(800)
  const [scale, setScale] = useState(1)
  const [pan, setPan] = useState(FIT_PAD / 2)
  const [follow, setFollow] = useState(true)
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null)

  const count = model.count

  // Measure the canvas width.
  useEffect(() => {
    const el = canvasRef.current
    if (!el) return
    const ro = new ResizeObserver(() => setWidth(el.clientWidth))
    ro.observe(el)
    setWidth(el.clientWidth)
    return () => ro.disconnect()
  }, [])

  // Fit-to-width on data / size change (only while the user hasn't interacted).
  useEffect(() => {
    if (count === 0) return
    if (!follow) return
    setScale(fitScale(width, count))
  }, [count, width, follow])

  // Auto-follow the latest node (default, requirement #4).
  useEffect(() => {
    if (!follow || count === 0) return
    const last = model.nodes[model.nodes.length - 1]
    const x = last.index * NODE_GAP * scale
    setPan(width - x - NODE_R)
  }, [count, scale, width, follow, model.nodes])

  const canvasHeight = Math.max(60, model.lanes.length * (LANE_H + LANE_GAP) + 16)

  const xOf = (index: number): number => index * NODE_GAP * scale + pan
  const yOfLane = (lane: number): number => 12 + lane * (LANE_H + LANE_GAP) + LANE_H / 2

  // Pan + zoom.
  const drag = useRef<{ startX: number; startPan: number } | null>(null)
  const onPointerDown = (e: React.PointerEvent) => {
    drag.current = { startX: e.clientX, startPan: pan }
  }
  const onPointerMove = (e: React.PointerEvent) => {
    if (!drag.current) return
    setFollow(false)
    setPan(drag.current.startPan + (e.clientX - drag.current.startX))
  }
  const onPointerUp = () => {
    drag.current = null
  }
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault()
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect()
    const cursorX = e.clientX - rect.left
    const indexAtCursor = (cursorX - pan) / (NODE_GAP * scale)
    const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15
    const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale * factor))
    setScale(next)
    setPan(cursorX - indexAtCursor * NODE_GAP * next)
  }

  const selectedEntry = useMemo(
    () => snapshot.trajectory.find((e) => e.seq === selectedSeq) ?? null,
    [snapshot.trajectory, selectedSeq],
  )

  if (model.nodes.length === 0) {
    return <div className={styles.empty}>{t('trajectory.noEvents')}</div>
  }

  // Viewport culling: only draw nodes within the visible x band.
  const culled = model.nodes.filter((n) => {
    const x = xOf(n.index)
    return x > -40 && x < width + 40
  })

  return (
    <div className={styles.root}>
      <div className={styles.toolbar}>
        <div className={styles.legend}>
          <span className={styles.legendTitle}>{t('trajectory.legend')}</span>
          {LEGEND.map((l) => (
            <span key={l.label} className={styles.legendItem}>
              <span className={styles.legendDot} style={{ background: l.color }} />
              {l.label}
            </span>
          ))}
        </div>
        <div className={styles.controls}>
          <button className={styles.btn} onClick={() => setFollow(true)} title={t('trajectory.backToLatest')}>
            {t('trajectory.backToLatest')}
          </button>
          <button className={styles.btn} onClick={() => setScale((s) => Math.max(MIN_SCALE, s / 1.2))} title={t('trajectory.zoomOut')}>
            −
          </button>
          <button className={styles.btn} onClick={() => setScale((s) => Math.min(MAX_SCALE, s * 1.2))} title={t('trajectory.zoomIn')}>
            +
          </button>
          <button
            className={styles.btn}
            onClick={() => {
              setScale(fitScale(width, count))
              setPan(FIT_PAD / 2)
              setFollow(true)
            }}
            title={t('trajectory.zoomReset')}
          >
            ⤢
          </button>
        </div>
      </div>

      <div
        ref={canvasRef}
        className={styles.canvas}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerLeave={onPointerUp}
        onWheel={onWheel}
      >
        <svg width={width} height={canvasHeight} className={styles.svg} data-testid="trajectory-timeline">
          {/* Turn boundaries — vertical markers, not nodes (issue #40). */}
          {model.turns.map((turn, i) => (
            <g key={`turn-${turn.turnId || i}`}>
              <line
                x1={xOf(turn.startIndex)}
                x2={xOf(turn.startIndex)}
                y1={4}
                y2={canvasHeight - 4}
                className={styles.turnMarker}
              />
              {turn.endIndex !== null && (
                <line
                  x1={xOf(turn.endIndex)}
                  x2={xOf(turn.endIndex)}
                  y1={4}
                  y2={canvasHeight - 4}
                  className={styles.turnMarkerEnd}
                />
              )}
              <text x={xOf(turn.startIndex) + 3} y={10} className={styles.turnLabel}>
                {turn.label}
              </text>
            </g>
          ))}

          {/* Lane guides */}
          {model.lanes.map((lane) => (
            <g key={lane.id}>
              <line
                x1={0}
                x2={width}
                y1={yOfLane(lane.depth === 0 ? 0 : lane.depth)}
                y2={yOfLane(lane.depth === 0 ? 0 : lane.depth)}
                className={styles.laneGuide}
              />
              <text x={6} y={yOfLane(lane.depth === 0 ? 0 : lane.depth) - 4} className={styles.laneLabel}>
                {lane.depth === 0 ? 'main' : lane.label.replace('task:', '')}
              </text>
            </g>
          ))}

          {/* Fork/join/depends edges */}
          {model.edges.map((edge, i) => {
            const from = model.nodes.find((n) => n.seq === edge.fromSeq)
            const to = model.nodes.find((n) => n.seq === edge.toSeq)
            if (!from || !to) return null
            const x1 = xOf(from.index)
            const x2 = xOf(to.index)
            if (x1 < -40 || x1 > width + 40 || x2 < -40 || x2 > width + 40) return null
            return (
              <path
                key={i}
                d={`M ${x1} ${yOfLane(from.lane)} C ${(x1 + x2) / 2} ${yOfLane(from.lane)}, ${(x1 + x2) / 2} ${yOfLane(to.lane)}, ${x2} ${yOfLane(to.lane)}`}
                className={styles.edge}
              />
            )
          })}

          {/* Nodes */}
          {culled.map((n) => {
            const x = xOf(n.index)
            const y = yOfLane(n.lane)
            const color = colorFor(n.kind)
            return (
              <g
                key={n.seq}
                className={styles.node}
                transform={`translate(${x}, ${y})`}
                data-kind={n.kind}
                data-seq={n.seq}
                onClick={() => setSelectedSeq(n.seq)}
              >
                {n.durationMs > 0 && n.kind === 'tool_calls' ? (
                  <rect
                    x={-NODE_R}
                    y={-NODE_R * 0.7}
                    width={NODE_R * 2}
                    height={NODE_R * 1.4}
                    rx={2}
                    fill={color}
                  />
                ) : (
                  <circle r={NODE_R} fill={color} />
                )}
                <title>{n.title}</title>
              </g>
            )
          })}
        </svg>
      </div>

      <div className={styles.detail}>
        <DetailPane entry={selectedEntry} />
      </div>
    </div>
  )
}

function fitScale(width: number, count: number): number {
  // Scale so `count - 1` inter-node gaps span the width, clamped to
  // [MIN_SCALE, MAX_FIT_SCALE]. The MAX_FIT_SCALE cap keeps a 2-node session
  // from spreading to an absurd per-node gap.
  const span = Math.max(1, count - 1)
  const target = Math.max(1, width - FIT_PAD)
  return Math.min(MAX_FIT_SCALE, Math.max(MIN_SCALE, target / span / NODE_GAP))
}
