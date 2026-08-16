// SettingsRoot — the settings modal. Three sections (left rail nav):
// General / Models / Permissions. DomainPacks + Skills were moved OUT to
// standalone modals (`/domainpack` · `/skills`) per the user's reorg.
//
// Models is the deepseek-harness-style provider manager: shows the config
// file path (web can't reveal Finder — shows the path + a copy button + a
// read-only TOML preview), lists configured providers with active/delete/
// set-active, and an add-provider form. Adds/deletes/switches go through the
// `provider/add` · `/delete` · `/set_active` RPCs, which write to
// `~/.oneai/config.toml` AND mutate the live `ProviderPool` (a new provider is
// immediately switchable from the composer's ModelSelect — no restart).
//
// Permissions is read-only (the active DomainPack's permission profile). When
// `config/get` failed (e.g. a stale app-server binary returning method-not-found),
// it shows the real error rather than a generic "offline".

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { Modal } from '../scenario/Modal'
import { useLocale } from '../i18n'
import type { SettingsStore } from './settingsStore'
import { useSettings } from './settingsStore'
import styles from './SettingsRoot.module.css'

type Section = 'general' | 'models' | 'permissions'

interface SettingsRootProps {
  store: SettingsStore
  theme: 'light' | 'dark'
  locale: 'zh' | 'en'
  planMode: boolean
  connection: 'connecting' | 'open' | 'closed' | 'error'
  onToggleTheme: () => void
  onToggleLocale: () => void
  onTogglePlan: () => void
  onClose: () => void
}

export function SettingsRoot({
  store,
  theme,
  locale,
  planMode,
  connection,
  onToggleTheme,
  onToggleLocale,
  onTogglePlan,
  onClose,
}: SettingsRootProps): ReactNode {
  const { t } = useLocale()
  const snap = useSettings(store)
  const [section, setSection] = useState<Section>('general')

  // Pull the read-only snapshots on open + whenever the modal re-opens.
  useEffect(() => {
    void store.refresh()
  }, [store])

  const sections: { id: Section; label: string }[] = [
    { id: 'general', label: t('settings.general') },
    { id: 'models', label: t('settings.models') },
    { id: 'permissions', label: t('settings.permissions') },
  ]

  return (
    <Modal title={t('settings.title')} width={920} onClose={onClose}>
      <div className={styles.layout}>
        <nav className={styles.rail}>
          {sections.map((s) => (
            <button
              key={s.id}
              className={`${styles.railItem} ${section === s.id ? styles.railItemActive : ''}`}
              onClick={() => setSection(s.id)}
            >
              {s.label}
            </button>
          ))}
        </nav>
        <div className={styles.content}>
          {section === 'general' && (
            <GeneralSection
              theme={theme}
              locale={locale}
              planMode={planMode}
              connection={connection}
              onToggleTheme={onToggleTheme}
              onToggleLocale={onToggleLocale}
              onTogglePlan={onTogglePlan}
            />
          )}
          {section === 'models' && <ModelsSection store={store} snap={snap} />}
          {section === 'permissions' && <PermissionsSection snap={snap} />}
        </div>
      </div>
    </Modal>
  )
}

function Row({ label, children }: { label: string; children: ReactNode }): ReactNode {
  return (
    <div className={styles.row}>
      <span className={styles.rowLabel}>{label}</span>
      <div className={styles.rowValue}>{children}</div>
    </div>
  )
}

function GeneralSection({
  theme,
  locale,
  planMode,
  connection,
  onToggleTheme,
  onToggleLocale,
  onTogglePlan,
}: {
  theme: 'light' | 'dark'
  locale: 'zh' | 'en'
  planMode: boolean
  connection: 'connecting' | 'open' | 'closed' | 'error'
  onToggleTheme: () => void
  onToggleLocale: () => void
  onTogglePlan: () => void
}): ReactNode {
  const { t } = useLocale()
  return (
    <div className={styles.stack}>
      <Row label={t('settings.theme')}>
        <button className={styles.toggle} onClick={onToggleTheme}>
          {theme === 'dark' ? '☾ ' + t('settings.dark') : '☀ ' + t('settings.light')}
        </button>
      </Row>
      <Row label={t('settings.language')}>
        <button className={styles.toggle} onClick={onToggleLocale}>
          {locale === 'zh' ? '中文' : 'English'}
        </button>
      </Row>
      <Row label={t('settings.planMode')}>
        <button className={styles.toggle} onClick={onTogglePlan}>
          {planMode ? t('settings.on') : t('settings.off')}
        </button>
        <span className={styles.hint}>{t('settings.planModeHint')}</span>
      </Row>
      <Row label={t('settings.connection')}>
        <span className={styles.value}>{t(`status.${connection}`)}</span>
      </Row>
    </div>
  )
}

function ModelsSection({
  store,
  snap,
}: {
  store: SettingsStore
  snap: ReturnType<typeof useSettings>
}): ReactNode {
  const { t } = useLocale()
  const [showAdd, setShowAdd] = useState(false)
  const [draft, setDraft] = useState({ name: '', kind: 'openai', api_key: '', base_url: '', model: '' })
  const [showConfig, setShowConfig] = useState(false)

  const submit = async () => {
    if (draft.name.trim().length === 0) return
    const ok = await store.providerAdd({
      name: draft.name.trim(),
      kind: draft.kind.trim().length > 0 ? draft.kind.trim() : undefined,
      api_key: draft.api_key.trim().length > 0 ? draft.api_key.trim() : undefined,
      base_url: draft.base_url.trim().length > 0 ? draft.base_url.trim() : undefined,
      model: draft.model.trim().length > 0 ? draft.model.trim() : undefined,
    })
    if (ok) {
      setDraft({ name: '', kind: 'openai', api_key: '', base_url: '', model: '' })
      setShowAdd(false)
    }
  }

  return (
    <div className={styles.stack}>
      {snap.lastError !== null && (
        <div className={styles.errorBanner}>{snap.lastError}</div>
      )}

      {/* Config file affordance — path + copy + read-only TOML preview. */}
      {snap.configFile !== null && (
        <div className={styles.configFile}>
          <div className={styles.configFileHead}>
            <span className={styles.configFileLabel}>{t('settings.configPath')}</span>
            <div className={styles.configFileActions}>
              <button
                className={styles.miniBtn}
                onClick={() => void navigator.clipboard?.writeText(snap.configFile?.path ?? '')}
              >
                {t('settings.copyPath')}
              </button>
              <button className={styles.miniBtn} onClick={() => setShowConfig((v) => !v)}>
                {showConfig ? t('settings.hide') : t('settings.show')}
              </button>
            </div>
          </div>
          <code className={styles.configPath}>{snap.configFile.path}</code>
          {showConfig && (
            <pre className={styles.configPreview}>{snap.configFile.content}</pre>
          )}
        </div>
      )}

      {/* Provider list. */}
      {snap.providers.length === 0 && (
        <div className={styles.empty}>{t('settings.noProvider')}</div>
      )}
      {snap.providers.map((p, i) => (
        <div key={i} className={`${styles.card} ${p.active ? styles.cardActive : ''}`}>
          <div className={styles.packHead}>
            <code className={styles.code}>{p.kind}</code>
            {p.active && <span className={styles.activeBadge}>{t('settings.active')}</span>}
            <span className={styles.providerModel}>{p.model || t('settings.inherited')}</span>
          </div>
          {p.base_url !== undefined && p.base_url !== '' && (
            <div className={styles.packDesc}>{p.base_url}</div>
          )}
          <div className={styles.skillActions}>
            <button
              className={styles.miniBtn}
              onClick={() => void store.providerSetActive(p.kind)}
              disabled={p.active}
            >
              {t('settings.setActive')}
            </button>
            <button
              className={`${styles.miniBtn} ${styles.danger}`}
              onClick={() => void store.providerDelete(p.kind)}
              disabled={snap.providers.length <= 1}
            >
              {t('settings.delete')}
            </button>
          </div>
        </div>
      ))}

      {/* Add provider form. */}
      {showAdd ? (
        <div className={styles.addForm}>
          <div className={styles.addRow}>
            <input
              className={styles.input}
              placeholder={t('settings.fldName')}
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            <input
              className={styles.input}
              placeholder={t('settings.fldKind')}
              value={draft.kind}
              onChange={(e) => setDraft({ ...draft, kind: e.target.value })}
            />
          </div>
          <input
            className={styles.input}
            placeholder={t('settings.fldApiKey')}
            value={draft.api_key}
            onChange={(e) => setDraft({ ...draft, api_key: e.target.value })}
          />
          <input
            className={styles.input}
            placeholder={t('settings.fldBaseUrl')}
            value={draft.base_url}
            onChange={(e) => setDraft({ ...draft, base_url: e.target.value })}
          />
          <input
            className={styles.input}
            placeholder={t('settings.fldModel')}
            value={draft.model}
            onChange={(e) => setDraft({ ...draft, model: e.target.value })}
          />
          <div className={styles.skillActions}>
            <button className={styles.miniBtn} onClick={() => void submit()}>
              {t('settings.save')}
            </button>
            <button className={styles.miniBtn} onClick={() => setShowAdd(false)}>
              {t('settings.cancel')}
            </button>
          </div>
        </div>
      ) : (
        <button className={styles.addToggle} onClick={() => setShowAdd(true)}>
          + {t('settings.addProvider')}
        </button>
      )}
    </div>
  )
}

function PermissionsSection({ snap }: { snap: ReturnType<typeof useSettings> }): ReactNode {
  const { t } = useLocale()
  const cfg = snap.config
  if (cfg === null) {
    // Show the real error if there is one (a stale binary returns
    // method-not-found — the user sees what to fix), else the generic offline
    // banner.
    return (
      <div className={styles.empty}>
        {snap.lastError !== null ? snap.lastError : t('settings.offline')}
      </div>
    )
  }
  return (
    <div className={styles.stack}>
      <Row label={t('settings.permissionProfile')}>
        <code className={styles.code}>{cfg.permission_profile ?? t('settings.none')}</code>
      </Row>
      <Row label={t('settings.activeDomain')}>
        <code className={styles.code}>{cfg.domain_pack ?? t('settings.none')}</code>
      </Row>
      <Row label={t('settings.planMode')}>
        <span className={styles.value}>{cfg.plan_mode ? t('settings.on') : t('settings.off')}</span>
      </Row>
    </div>
  )
}
