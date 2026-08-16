// SidebarRoot — the left column. Logo/New-session/sessions/scenarios/footer.
// W1 wires: New Session (session/create), session list (session/list), theme
// toggle. The Scenarios section is a placeholder for the W3 scenario entry.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { SessionInfo } from '../rpc/types'
import { SessionList } from './SessionList'
import styles from './SidebarRoot.module.css'

interface SidebarRootProps {
  sessions: SessionInfo[]
  currentSessionId: string | null
  theme: 'light' | 'dark'
  onNewSession: () => void
  onPickSession: (id: string) => void
  onToggleTheme: () => void
  onToggleLocale: () => void
}

export function SidebarRoot({
  sessions,
  currentSessionId,
  theme,
  onNewSession,
  onPickSession,
  onToggleTheme,
  onToggleLocale,
}: SidebarRootProps): ReactNode {
  const { t, locale } = useLocale()
  return (
    <div className={styles.root}>
      <div className={styles.header}>
        <span className={styles.logo}>◆</span>
        <button className={styles.newBtn} onClick={onNewSession}>
          {t('sidebar.new')}
        </button>
      </div>

      <div className={styles.scroll}>
        <SectionLabel>{t('sidebar.sessions')}</SectionLabel>
        <SessionList
          sessions={sessions}
          currentId={currentSessionId}
          onPick={onPickSession}
        />

        <SectionLabel>{t('sidebar.scenarios')}</SectionLabel>
        <div className={styles.scenarioPlaceholder}>
          {/* W3: ScenarioEntry — pick a scenario → topic intake → group chat. */}
          <span className={styles.comingSoon}>W3</span>
        </div>
      </div>

      <div className={styles.footer}>
        <button className={styles.footBtn} onClick={onToggleTheme} title={t('theme.toggle')}>
          {theme === 'dark' ? '☀' : '☾'}
        </button>
        <button className={styles.footBtn} onClick={onToggleLocale} title="language">
          {locale === 'zh' ? '中' : 'EN'}
        </button>
      </div>
    </div>
  )
}

function SectionLabel({ children }: { children: ReactNode }): ReactNode {
  return <div className={styles.sectionLabel}>{children}</div>
}
