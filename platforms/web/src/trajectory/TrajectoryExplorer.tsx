// TrajectoryExplorer — the resident full-area trajectory view (issue #40).
//
// A horizontal swim-lane timeline: lane 0 = main agent, one lane per delegated
// sub-agent; nodes are color-coded markers positioned by wall-clock time;
// fork/join/depends edges draw the delegation DAG. Drag to pan, wheel to zoom,
// click a node for its type-specific detail below. Auto-follows the latest
// node until the user pans away ("回到最新" re-enables).

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

const LANE_H = 26
const LANE_GAP = 8
const NODE_R = 5
const MIN_SCALE = 0.005
const MAX_SCALE = 2

export function TrajectoryExplorer({ snapshot }: TrajectoryExplorerProps): ReactNode {
  const { t } = useLocale()
  const model = useMemo(() => buildTimeline(snapshot.trajectory), [snapshot.trajectory])

  const canvasRef = useRef<HTMLDivElement | null>(null)
  const [width, setWidth] = useState(800)
  const [scale, setScale] = useState(0.05)
  const [pan, setPan] = useState(0)
  const [follow, setFollow] = useState(true)
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null)

  const rangeMs = model.timeRange[1] - model.timeRange[0]

  // Measure the canvas width.
  useEffect(() => {
    const el = canvasRef.current
    if (!el) return
    const ro = new ResizeObserver(() => setWidth(el.clientWidth))
    ro.observe(el)
    setWidth(el.clientWidth)
    return () => ro.disconnect()
  }, [])

  // Fit on first data (only while the user hasn't interacted).
  useEffect(() => {
    if (model.nodes.length === 0) return
    if (!follow) return
    const fit = Math.min(0.2, Math.max(MIN_SCALE, width / Math.max(1, rangeMs)))
    setScale(fit)
  }, [model.nodes.length, rangeMs, width, follow])

  // Auto-follow the latest node (default, requirement #4).
  useEffect(() => {
    if (!follow || model.nodes.length === 0) return
    const last = model.nodes[model.nodes.length - 1]
    const x = (last.at - model.timeRange[0]) * scale
    setPan(width - x - NODE_R)
  }, [model.nodes.length, scale, width, follow, model.timeRange, model.nodes])

  const canvasHeight = Math.max(60, model.lanes.length * (LANE_H + LANE_GAP) + 12)

  const xOf = (at: number): number => (at - model.timeRange[0]) * scale + pan
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
    const timeAtCursor = (cursorX - pan) / scale + model.timeRange[0]
    const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15
    const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale * factor))
    setScale(next)
    setPan(cursorX - (timeAtCursor - model.timeRange[0]) * next)
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
    const x = xOf(n.at)
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
              setScale(Math.min(0.2, Math.max(MIN_SCALE, width / Math.max(1, rangeMs))))
              setPan(0)
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
              <text x={6} y={yOfLane(lane.depth === 0 ? 0 : lane.depth) - 8} className={styles.laneLabel}>
                {lane.depth === 0 ? 'main' : lane.label.replace('task:', '')}
              </text>
            </g>
          ))}

          {/* Fork/join/depends edges */}
          {model.edges.map((edge, i) => {
            const from = model.nodes.find((n) => n.seq === edge.fromSeq)
            const to = model.nodes.find((n) => n.seq === edge.toSeq)
            if (!from || !to) return null
            const x1 = xOf(from.at)
            const x2 = xOf(to.at)
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
            const x = xOf(n.at)
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
                    y={-NODE_R}
                    width={Math.max(10, n.durationMs * scale)}
                    height={NODE_R * 2}
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
