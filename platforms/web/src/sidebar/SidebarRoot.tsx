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
import { Tooltip } from '../components/Tooltip'
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
  onOpenSettings: () => void
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
  onOpenSettings,
}: SidebarRootProps): ReactNode {
  const { t, locale } = useLocale()
  // Brand mark is a single B/W transparent PNG (the light-mode asset). Dark
  // mode reuses the same image with `filter: invert(1)` — since the mark is
  // pure black/white, inversion flips it cleanly and transparent areas stay
  // transparent. This avoids the separate dark-mode assets, which render
  // incorrectly.
  const brandPicSrc = '/brand/ic_pic_white.png'
  const brandAlphaSrc = '/brand/ic_alpha_white.png'
  const brandFilter = theme === 'dark' ? 'invert(1)' : 'none'
  return (
    <div className={styles.root}>
      <div className={styles.header}>
        <div className={styles.brandMark}>
          <img
            className={styles.brandPic}
            src={brandPicSrc}
            alt="OneAI"
            draggable={false}
            style={{ filter: brandFilter }}
          />
          <img
            className={styles.brandAlpha}
            src={brandAlphaSrc}
            alt="OneAI"
            draggable={false}
            style={{ filter: brandFilter }}
          />
        </div>
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
          scenarios={scenarios}
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
        <Tooltip label={t('theme.toggle')} side="top">
          <button className={styles.footBtn} onClick={onToggleTheme} aria-label={t('theme.toggle')}>
            {theme === 'dark' ? '☀' : '☾'}
          </button>
        </Tooltip>
        <Tooltip label={locale === 'zh' ? 'English' : '中文'} side="top">
          <button className={styles.footBtn} onClick={onToggleLocale} aria-label="language">
            {locale === 'zh' ? '中' : 'EN'}
          </button>
        </Tooltip>
        <Tooltip label={t('sidebar.settings')} side="top">
          <button className={styles.footBtn} onClick={onOpenSettings} aria-label={t('sidebar.settings')}>
            <svg width="18" height="18" viewBox="0 0 24 24" aria-hidden focusable="false">
              <path
                fill="currentColor"
                d="M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58c.18-.14.23-.41.12-.61l-1.92-3.32c-.12-.22-.37-.29-.59-.22l-2.39.96a7.5 7.5 0 0 0-1.62-.94l-.36-2.54a.48.48 0 0 0-.48-.41H9.6a.48.48 0 0 0-.47.41l-.36 2.54c-.59.24-1.13.56-1.62.94l-2.39-.96c-.22-.08-.47 0-.59.22L2.25 8.13c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.07.62-.07.94 0 .32.02.64.07.94l-2.03 1.58c-.18.14-.23.41-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.04.24.24.41.48.41h3.84c.24 0 .43-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.21.08-.47-.12-.61l-2.03-1.58zM12 15.6a3.6 3.6 0 1 1 0-7.2 3.6 3.6 0 0 1 0 7.2z"
              />
            </svg>
          </button>
        </Tooltip>
      </div>
    </div>
  )
}

function SectionLabel({ children }: { children: ReactNode }): ReactNode {
  return <div className={styles.sectionLabel}>{children}</div>
}
