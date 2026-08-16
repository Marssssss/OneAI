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

