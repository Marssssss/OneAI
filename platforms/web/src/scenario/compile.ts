// Scenario compile — the single source of truth for turning a rich
// `BusScenario` (+ collected topic values) into the engine launch payload
// `BusGroupScenario`. Mirrors macOS `ChatViewModel.buildGroupScenarioJSON` /
// `Scenario.specView`: bake the visible topic fields into each member's
// `system_prompt` as a `【场景背景】`/`[Scenario Background]` block (per
// `visible_to`), fold every non-blank value into the title suffix, and drop
// the UI-only fields the engine does not consume (`icon`, `name`, `role`,
// `topic_fields`, `debrief`). Sharing it here keeps every frontend's compile
// identical — the engine build re-checks on the parsed config as
// defense-in-depth.
//
// NOTE: this fn does NOT re-validate — the caller validates first via
// `scenario/validate`. It only maps fields.

import type { BusGroupScenario, BusLocale, BusScenario } from '../rpc/types'

export interface TopicPair {
  label: string
  value: string
  /** Member ids allowed to see this field's value. undefined ⇒ all. */
  visibleTo?: string[]
}

const ZH_BG_HEADER = '【场景背景】'
const EN_BG_HEADER = '[Scenario Background]'

/** Collect non-blank topic values into (label, value, visibleTo) pairs. */
export function collectTopicPairs(
  scenario: BusScenario,
  values: Record<string, string>,
): TopicPair[] {
  const fields = scenario.topic_fields ?? []
  const out: TopicPair[] = []
  for (const f of fields) {
    const v = (values[f.id] ?? '').trim()
    if (v.length === 0) continue
    out.push({ label: f.label, value: v, visibleTo: f.visible_to })
  }
  return out
}

/** The first user message built from collected topic values (for scenarios
 *  with no opener — the user's topic kicks the first round). Joins non-blank
 *  values with " · ". Empty when there are no topic values. */
export function firstUserMessage(
  scenario: BusScenario,
  values: Record<string, string>,
): string {
  const pairs = collectTopicPairs(scenario, values)
  return pairs.map((p) => p.value).join(' · ')
}

/** Title for the running conversation: the scenario name suffixed by the
 *  non-blank topic values ("name · v1 · v2"); bare name when none. */
export function scenarioTitle(
  scenario: BusScenario,
  values: Record<string, string>,
): string {
  const pairs = collectTopicPairs(scenario, values)
  if (pairs.length === 0) return scenario.name
  return `${scenario.name}·${pairs.map((p) => p.value).join('·')}`
}

function backgroundBlock(pairs: TopicPair[], memberId: string, locale: BusLocale): string {
  const visible = pairs.filter((p) => {
    if (p.visibleTo === undefined) return true
    return p.visibleTo.includes(memberId)
  })
  if (visible.length === 0) return ''
  const header = locale === 'en' ? EN_BG_HEADER : ZH_BG_HEADER
  const lines = visible.map((p) => `${p.label}: ${p.value}`).join('\n')
  return `${header}\n${lines}`
}

/** Compile a rich scenario + collected topic values into the `group/start`
 *  payload. Bakes the visible topic background into each member's
 *  `system_prompt` and drops UI-only fields. */
export function compileGroupScenario(
  scenario: BusScenario,
  values: Record<string, string>,
  locale: BusLocale,
): BusGroupScenario {
  const pairs = collectTopicPairs(scenario, values)
  const members = scenario.members.map((m) => {
    const bg = backgroundBlock(pairs, m.id, locale)
    const system_prompt =
      bg.length === 0 ? m.system_prompt : `${m.system_prompt}\n\n${bg}`
    return {
      id: m.id,
      name: m.name,
      system_prompt,
      kind: m.kind,
      model: m.model,
      api_key: m.api_key,
      base_url: m.base_url,
      color: m.color,
      avatar: m.avatar,
    }
  })
  return {
    members,
    turn_policy: scenario.turn_policy,
    script_order: scenario.script_order,
    moderator_id: scenario.moderator_id,
    opener_agent_id: scenario.opener_agent_id,
    opener_line: scenario.opener_line,
    title: scenarioTitle(scenario, values),
    review_loop: scenario.review_loop,
    locale,
  }
}
