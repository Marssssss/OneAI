// Markdown — streaming-capable markdown rendering.
//
// W1 uses react-markdown + remark-gfm + rehype-highlight (highlight.js). The
// proper incremental renderer (over micromark, parsing only the new tail on
// each delta) is a W2/W5 refinement flagged in the design doc. To keep
// streaming smooth here, the ChatView memoizes this component per final text
// and re-renders it at most at the StreamCoalescer's 20fps cadence — so the
// re-parse cost is bounded by the coalescer, not by the token rate.
import { memo } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import rehypeHighlight from 'rehype-highlight'
import 'highlight.js/styles/github-dark.css'

interface MarkdownProps {
  text: string
}

export const Markdown = memo(function Markdown({ text }: MarkdownProps) {
  return (
    <div className="oneai-md">
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
        {text}
      </ReactMarkdown>
    </div>
  )
})
