import { describe, expect, it } from 'vitest'
import {
  collectTopicPairs,
  compileGroupScenario,
  scenarioTitle,
} from './compile'
import { presetsFor } from './presets'
import type { BusScenario } from '../rpc/types'

// compileGroupScenario turns an editor BusScenario + topic values into the
// engine's BusGroupScenario: members get a background block appended to their
// system_prompt, visible_to is respected, title is built from topic values.

describe('compileGroupScenario', () => {
  const interview = presetsFor('zh').find((s) => s.id === 'preset-interview')!
  const values = {
    position: '前端工程师',
    company: '字节跳动',
    level: '社招 P5',
    projects: '电商订单中台',
  }

  it('builds a title from scenario name + topic values joined by ·', () => {
    expect(scenarioTitle(interview, values)).toBe(
      '面试演练·前端工程师·字节跳动·社招 P5·电商订单中台',
    )
  })

  it('appends a background block to each member system_prompt', () => {
    const compiled = compileGroupScenario(interview, values, 'zh')
    const interviewer = compiled.members.find((m) => m.id === 'interviewer')!
    const coach = compiled.members.find((m) => m.id === 'coach')!

    // interviewer (not in any visible_to) sees the non-project fields, not the
    // project field (projects is visible only to coach).
    expect(interviewer.system_prompt).toContain('应聘岗位: 前端工程师')
    expect(interviewer.system_prompt).toContain('目标公司: 字节跳动')
    expect(interviewer.system_prompt).not.toContain('项目经历')
    // The original prompt is preserved as the prefix.
    expect(interviewer.system_prompt.startsWith('你是一名资深技术面试官')).toBe(true)

    // coach (in projects.visible_to) additionally sees the project field.
    expect(coach.system_prompt).toContain('项目经历: 电商订单中台')
  })

  it('echoes scenario structural fields verbatim', () => {
    const compiled = compileGroupScenario(interview, values, 'zh')
    expect(compiled.turn_policy).toBe('scripted')
    expect(compiled.script_order).toEqual(['coach', 'interviewer'])
    expect(compiled.opener_agent_id).toBe('interviewer')
    expect(compiled.opener_line).toBe('我们开始面试吧。请先做个简短的自我介绍。')
    expect(compiled.locale).toBe('zh')
    expect(compiled.members).toHaveLength(2)
  })

  it('collectTopicPairs maps field→value, skipping empties', () => {
    const pairs = collectTopicPairs(interview, {
      position: '前端',
      company: '',
      level: 'P5',
      projects: '',
    })
    expect(pairs.map((p) => p.label)).toEqual(['应聘岗位', '职位级别'])
    expect(pairs.map((p) => p.value)).toEqual(['前端', 'P5'])
  })

  it('leaves system_prompt untouched when there are no topic values', () => {
    const noTopic: BusScenario = {
      ...interview,
      topic_fields: undefined,
    } as BusScenario
    const compiled = compileGroupScenario(noTopic, {}, 'zh')
    const interviewer = compiled.members.find((m) => m.id === 'interviewer')!
    expect(interviewer.system_prompt).toBe(
      interview.members.find((m) => m.id === 'interviewer')!.system_prompt,
    )
  })
})
