// SidebarRoot — the left column. Logo/New-session/sessions/scenarios/footer.
// W1 wires: New Session (session/create), session list (session/list), theme
// toggle. W3 wires the scenario section: each scenario row is a direct entry
// (click → topic intake → group chat); a per-row "⋯" opens edit/view; "+ 新建
// 场景" sits at the bottom of the list. Session rows carry their own "⋯" menu
// (rename / archive / delete) — see SessionList.

import type { ReactNode } from 'react'
import { useLocale } from '../i18n'
import type { BusScenario, SessionInfo } from '../rpc/types'
import type { ScenarioEntry } from '../scenario/scenarioStore'
import type { SessionMeta } from '../store/sessionMeta'
import { MoreMenu } from '../components/MoreMenu'
import { SessionList } from './SessionList'
import styles from './SidebarRoot.module.css'

interface SidebarRootProps {
  sessions: SessionInfo[]
  currentSessionId: string | null
  theme: 'light' | 'dark'
  scenarios: ScenarioEntry[]
  sessionMeta: SessionMeta
  onNewSession: () => void
  onPickSession: (id: string) => void
  onRenameSession: (id: string, currentTitle: string) => void
  onArchiveSession: (id: string) => void
  onUnarchiveSession: (id: string) => void
  onDeleteSession: (id: string) => void
  onToggleTheme: () => void
  onToggleLocale: () => void
  onPickScenario: (scenario: BusScenario) => void
  onNewScenario: () => void
  onEditScenario: (scenario: BusScenario) => void
}

export function SidebarRoot({
  sessions,
  currentSessionId,
  theme,
  scenarios,
  sessionMeta,
  onNewSession,
  onPickSession,
  onRenameSession,
  onArchiveSession,
  onUnarchiveSession,
  onDeleteSession,
  onToggleTheme,
  onToggleLocale,
  onPickScenario,
  onNewScenario,
  onEditScenario,
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
          meta={sessionMeta}
          onPick={onPickSession}
          onRename={onRenameSession}
          onArchive={onArchiveSession}
          onUnarchive={onUnarchiveSession}
          onDelete={onDeleteSession}
        />

        <SectionLabel>{t('sidebar.scenarios')}</SectionLabel>
        <div className={styles.scenarioList}>
          {scenarios.length === 0 && (
            <div className={styles.scenarioEmpty}>{t('scenario.empty')}</div>
          )}
          {scenarios.map((e) => (
            <div className={styles.scenarioItem} key={e.scenario.id}>
              <button
                className={styles.scenarioPick}
                onClick={() => onPickScenario(e.scenario)}
                title={e.scenario.name}
              >
                <span className={styles.scenarioIcon}>{e.scenario.icon ?? '◆'}</span>
                <span className={styles.scenarioName}>{e.scenario.name}</span>
              </button>
              <MoreMenu
                items={[{ id: 'edit', label: e.isPreset ? t('scenario.view') : t('scenario.edit') }]}
                onPick={() => onEditScenario(e.scenario)}
                ariaLabel={t('scenario.edit')}
              />
            </div>
          ))}
          <button className={styles.scenarioNew} onClick={onNewScenario}>
            + {t('scenario.new')}
          </button>
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
