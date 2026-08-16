// IncrementalMarkdown — block-keyed incremental markdown renderer over
// micromark/mdast + shiki highlighting.
//
// Why not re-parse the whole string per token into one React tree? It reflows
// already-complete content (a finished code fence would re-highlight on every
// appended token of an unrelated later paragraph) and floods the reconciler.
// Instead:
//  - Parse the full text to mdast once per coalescer flush (cheap, bounded to
//    20fps). The expensive work is the React reconciliation + shiki highlight.
//  - Render each *top-level block* through a `memo`'d `<Block>` keyed by a
//    stable content hash. A completed block (unchanged hash) reuses its
//    already-rendered React subtree — `React.memo` short-circuits the render.
//    Only the streaming tail block (whose hash changes every token) re-renders
//    and re-highlights.
//
// This realizes the design-doc intent ("复用未变子树, 仅重渲染变更节点") without a
// fragile mdast tree-diff: block-keyed memoization is the practical realization
// of subtree reuse. shiki inits asynchronously (it loads grammar/theme chunks
// on first use); `<CodeBlock>` renders plain code until the highlighter
// resolves, then swaps to highlighted HTML — non-blocking.

import { memo, useEffect, useMemo, useRef, useState } from 'react'
import type { ElementType, ReactNode } from 'react'
import { fromMarkdown } from 'mdast-util-from-markdown'
import { gfm } from 'micromark-extension-gfm'
import { gfmFromMarkdown } from 'mdast-util-gfm'
import { createHighlighter, type Highlighter } from 'shiki'

// mdast is structurally typed here (a loose shape) so we don't take a hard
// dependency on `@types/mdast`; the fields the walker reads are the only ones
// that matter.
interface MdNode {
  type: string
  value?: string
  lang?: string
  ordered?: boolean
  depth?: number
  url?: string
  alt?: string
  children?: MdNode[]
  [k: string]: unknown
}

const SHIKI_LANGS = ['typescript', 'bash', 'json', 'python', 'rust', 'html', 'css', 'javascript'] as const
const SHIKI_THEMES = ['github-dark', 'github-light'] as const

let highlighterPromise: Promise<Highlighter> | null = null
function getHighlighter(): Promise<Highlighter> {
  if (highlighterPromise === null) {
    highlighterPromise = createHighlighter({
      langs: [...SHIKI_LANGS],
      themes: [...SHIKI_THEMES],
    })
  }
  return highlighterPromise
}

function parseMd(text: string): MdNode[] {
  const tree = fromMarkdown(text, {
    extensions: [gfm()],
    mdastExtensions: [gfmFromMarkdown()],
  })
  return (tree as unknown as MdNode).children ?? []
}

// Stable content hash (djb2) — used as the React key so a block reuses its
// component instance across flushes, AND as the React.memo comparison so an
// unchanged block short-circuits the render.
function hashNode(node: MdNode): string {
  const s = JSON.stringify(node)
  let h = 5381
  for (let i = 0; i < s.length; i += 1) {
    h = ((h << 5) + h + s.charCodeAt(i)) | 0
  }
  // unsigned base36, prefix to avoid leading-digit collisions
  return `b${(h >>> 0).toString(36)}`
}

interface MarkdownProps {
  text: string
  theme: 'light' | 'dark'
}

export const IncrementalMarkdown = memo(function IncrementalMarkdown({
  text,
  theme,
}: MarkdownProps): ReactNode {
  const blocks = useMemo(() => parseMd(text), [text])
  return (
    <div className="oneai-md" data-md-theme={theme}>
      {blocks.map((node, i) => (
        <Block key={hashNode(node) + ':' + i} node={node} theme={theme} />
      ))}
    </div>
  )
})

const Block = memo(
  function Block({ node, theme }: { node: MdNode; theme: 'light' | 'dark' }): ReactNode {
    return renderBlock(node, theme)
  },
  (a, b) => hashNode(a.node) === hashNode(b.node) && a.theme === b.theme,
)

// ── block-level render ───────────────────────────────────────────────────────

function renderBlock(node: MdNode, theme: 'light' | 'dark'): ReactNode {
  switch (node.type) {
    case 'paragraph':
      return <p>{renderInline(node.children ?? [])}</p>
    case 'heading': {
      const Tag = `h${Math.min(6, Math.max(1, node.depth ?? 1))}` as ElementType
      return <Tag>{renderInline(node.children ?? [])}</Tag>
    }
    case 'list':
      return node.ordered ? (
        <ol>{(node.children ?? []).map((c, i) => <li key={i}>{renderBlock(c, theme)}</li>)}</ol>
      ) : (
        <ul>{(node.children ?? []).map((c, i) => <li key={i}>{renderBlock(c, theme)}</li>)}</ul>
      )
    case 'listItem':
      // mdast listItem children may include a nested paragraph; render the
      // phrasing content of its children directly.
      return <>{renderInline(flattenPhrasing(node.children ?? []))}</>
    case 'blockquote':
      return <blockquote>{(node.children ?? []).map((c, i) => <div key={i}>{renderBlock(c, theme)}</div>)}</blockquote>
    case 'code':
      return <CodeBlock code={node.value ?? ''} lang={node.lang ?? ''} theme={theme} />
    case 'table':
      return renderTable(node)
    case 'thematicBreak':
      return <hr />
    case 'html':
      return <div dangerouslySetInnerHTML={{ __html: node.value ?? '' }} />
    default:
      return node.value ? <p>{node.value}</p> : null
  }
}

function renderTable(node: MdNode): ReactNode {
  const rows = (node.children ?? []) as MdNode[]
  const head = rows.find((r) => r.type === 'tableRow')
  const body = rows.filter((r) => r.type === 'tableRow').slice(1)
  const cells = (r: MdNode): MdNode[] => (r.children ?? []).filter((c) => c.type === 'tableCell')
  return (
    <table>
      {head && (
        <thead>
          <tr>
            {cells(head).map((c, i) => (
              <th key={i}>{renderInline(c.children ?? [])}</th>
            ))}
          </tr>
        </thead>
      )}
      <tbody>
        {body.map((r, ri) => (
          <tr key={ri}>
            {cells(r).map((c, ci) => (
              <td key={ci}>{renderInline(c.children ?? [])}</td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  )
}

// ── inline (phrasing) render ─────────────────────────────────────────────────

function renderInline(nodes: MdNode[]): ReactNode {
  return nodes.map((node, i) => {
    switch (node.type) {
      case 'text':
        return <span key={i}>{node.value ?? ''}</span>
      case 'strong':
        return <strong key={i}>{renderInline(node.children ?? [])}</strong>
      case 'emphasis':
        return <em key={i}>{renderInline(node.children ?? [])}</em>
      case 'delete':
        return <del key={i}>{renderInline(node.children ?? [])}</del>
      case 'inlineCode':
        return <code key={i} className="oneai-md-inline-code">{node.value ?? ''}</code>
      case 'link':
        return (
          <a key={i} href={node.url ?? '#'} target="_blank" rel="noreferrer noopener">
            {renderInline(node.children ?? [])}
          </a>
        )
      case 'image':
        return <img key={i} src={node.url ?? ''} alt={node.alt ?? ''} />
      case 'break':
        return <br key={i} />
      default:
        return <span key={i}>{node.value ?? ''}</span>
    }
  })
}

function flattenPhrasing(nodes: MdNode[]): MdNode[] {
  // A listItem's children are often [{paragraph: {children:[...]}}]; unwrap one
  // level of paragraph so inline content renders without a stray <p>.
  const out: MdNode[] = []
  for (const n of nodes) {
    if (n.type === 'paragraph') out.push(...(n.children ?? []))
    else out.push(n)
  }
  return out
}

// ── code block (shiki, async-gated) ──────────────────────────────────────────

function CodeBlock({
  code,
  lang,
  theme,
}: {
  code: string
  lang: string
  theme: 'light' | 'dark'
}): ReactNode {
  const [html, setHtml] = useState<string | null>(null)
  const hlRef = useRef<Highlighter | null>(null)
  const supported = (SHIKI_LANGS as readonly string[]).includes(lang) ? lang : ''

  useEffect(() => {
    let cancelled = false
    if (supported === '') {
      setHtml(null)
      return
    }
    const run = async () => {
      const hl = hlRef.current ?? (await getHighlighter())
      if (cancelled) return
      hlRef.current = hl
      const shikiTheme = theme === 'dark' ? 'github-dark' : 'github-light'
      try {
        setHtml(hl.codeToHtml(code, { lang: supported, theme: shikiTheme }))
      } catch {
        setHtml(null)
      }
    }
    void run()
    return () => {
      cancelled = true
    }
  }, [code, supported, theme])

  if (html !== null) {
    return (
      <pre className="oneai-md-code" dangerouslySetInnerHTML={{ __html: html }} />
    )
  }
  return (
    <pre className="oneai-md-code oneai-md-code-plain">
      <code>{code}</code>
    </pre>
  )
}
