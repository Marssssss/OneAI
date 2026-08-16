// Minimal i18n store — locale-driven re-render via an external store, so a
// `t()` reference change makes memoized components re-render naturally
// (mirrors dsh's `LocaleFace`). W1 ships zh/en; new locales are additive.

import { useSyncExternalStore } from 'react'

export type Locale = 'zh' | 'en'

const STRINGS: Record<Locale, Record<string, string>> = {
  zh: {
    'app.title': 'OneAI',
    'sidebar.new': '新对话',
    'sidebar.sessions': '会话',
    'sidebar.scenarios': '场景',
    'sidebar.settings': '设置',
    'sidebar.empty': '暂无会话',
    'sidebar.unfinished': '未完成的工作',
    'composer.placeholder': '给 OneAI 发消息…（Enter 发送，Shift+Enter 换行）',
    'composer.send': '发送',
    'composer.stop': '停止',
    'chat.empty.title': '开始一段新对话',
    'chat.empty.subtitle': '在下方输入框给 OneAI 发消息',
    'chat.error': '出错了',
    'status.connecting': '正在连接引擎…',
    'status.open': '已连接',
    'status.closed': '未连接',
    'status.error': '连接错误',
    'theme.toggle': '切换主题',
    'thinking': '思考',
    'plan.mode': 'Plan 模式',
    'plan.on': '已开启',
    'plan.off': '关闭',
    'plan.steps': '执行计划',
    'command.plan': '切换 Plan 模式',
    'command.clear': '清空当前会话',
    'command.compact': '压缩对话历史',
    'command.hint': '斜杠命令',
    'tool.pending': '运行中',
    'tool.done': '完成',
    'tool.error': '出错',
    'tool.args': '参数',
    'tool.result': '结果',
    'tool.added': '新增工具',
    'tool.inspect': '在详情栏查看',
    'approval.waiting': '等待审批',
    'approval.queued': '排队中',
    'approval.allow': '允许',
    'approval.refuse': '拒绝',
    'approval.deny': '拒绝',
    'approval.accept': '采纳',
    'approval.revise': '修改',
    'approval.revise.placeholder': '说明需要修改的地方…',
    'approval.pick': '选择一个选项',
    'approval.allow.host': '允许（本会话）',
    'approval.decline': '不回答',
    'approval.cancel': '取消',
    'approval.submit': '提交',
    'details.title': '详情',
    'details.empty': '从对话中选择一个工具调用以查看详情',
    'details.tool': '工具',
    'details.args': '参数',
    'details.result': '结果',
  },
  en: {
    'app.title': 'OneAI',
    'sidebar.new': 'New chat',
    'sidebar.sessions': 'Sessions',
    'sidebar.scenarios': 'Scenarios',
    'sidebar.settings': 'Settings',
    'sidebar.empty': 'No sessions',
    'sidebar.unfinished': 'Unfinished work',
    'composer.placeholder': 'Message OneAI… (Enter to send, Shift+Enter for newline)',
    'composer.send': 'Send',
    'composer.stop': 'Stop',
    'chat.empty.title': 'Start a new conversation',
    'chat.empty.subtitle': 'Message OneAI in the box below',
    'chat.error': 'Something went wrong',
    'status.connecting': 'Connecting to engine…',
    'status.open': 'Connected',
    'status.closed': 'Disconnected',
    'status.error': 'Connection error',
    'theme.toggle': 'Toggle theme',
    'thinking': 'Thinking',
    'plan.mode': 'Plan mode',
    'plan.on': 'On',
    'plan.off': 'Off',
    'plan.steps': 'Plan',
    'command.plan': 'Toggle plan mode',
    'command.clear': 'Clear current session',
    'command.compact': 'Compact conversation',
    'command.hint': 'Slash commands',
    'tool.pending': 'Running',
    'tool.done': 'Done',
    'tool.error': 'Error',
    'tool.args': 'Args',
    'tool.result': 'Result',
    'tool.added': 'Added tools',
    'tool.inspect': 'Inspect in details',
    'approval.waiting': 'Waiting for approval',
    'approval.queued': 'queued',
    'approval.allow': 'Allow',
    'approval.refuse': 'Refuse',
    'approval.deny': 'Deny',
    'approval.accept': 'Accept',
    'approval.revise': 'Revise',
    'approval.revise.placeholder': 'Describe what to change…',
    'approval.pick': 'Pick an option',
    'approval.allow.host': 'Allow (this session)',
    'approval.decline': 'Decline',
    'approval.cancel': 'Cancel',
    'approval.submit': 'Submit',
    'details.title': 'Details',
    'details.empty': 'Select a tool call from the chat to inspect',
    'details.tool': 'Tool',
    'details.args': 'Args',
    'details.result': 'Result',
  },
}

class LocaleStore {
  private locale: Locale = 'zh'
  private listeners = new Set<() => void>()

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn)
    return () => this.listeners.delete(fn)
  }
  setLocale(l: Locale): void {
    if (l === this.locale) return
    this.locale = l
    for (const fn of this.listeners) fn()
  }
  getLocale(): Locale {
    return this.locale
  }
  t(): (key: string) => string {
    // Return a fresh closure on each locale change so memoized components
    // that capture `t` re-render when the locale flips.
    const table = STRINGS[this.locale]
    return (key: string) => table[key] ?? key
  }
}

export const localeStore = new LocaleStore()

export function useLocale(): {
  locale: Locale
  t: (key: string) => string
  setLocale: (l: Locale) => void
} {
  const locale = useSyncExternalStore(
    localeStore.subscribe,
    () => localeStore.getLocale(),
    () => 'zh' as Locale,
  )
  return { locale, t: localeStore.t(), setLocale: (l) => localeStore.setLocale(l) }
}

