// ScenarioErrorLocalizer — turn a `ScenarioError.code` into a localized
// message. The engine returns English `message` as a fallback; the stable
// `code` lets every frontend render its own translation instead of baking in
// the English text. Mirrors macOS `ScenarioErrorLocalizer`.
//
// Codes (from BusGroupScenario::validate / BusScenario::validate):
//   empty       — a required collection/string is missing (members, name…)
//   unknown_id  — an id reference points to a non-existent member
//   missing     — a required id field is blank (moderator_id)
//   invalid     — a numeric/enum value is out of range (max_rounds)

import type { Locale } from '../i18n'
import type { ScenarioError } from '../rpc/types'

const TABLE: Record<Locale, Record<string, string>> = {
  zh: {
    empty: '不能为空',
    unknown_id: '引用了不存在的成员',
    missing: '该字段必填',
    invalid: '取值无效',
  },
  en: {
    empty: 'cannot be empty',
    unknown_id: 'references an unknown member',
    missing: 'this field is required',
    invalid: 'invalid value',
  },
}

/** Localize a single error's code; falls back to the engine's English
 *  `message` when the code is unrecognized. */
export function localizeError(err: ScenarioError, locale: Locale): string {
  const t = TABLE[locale][err.code]
  return t ?? err.message
}

/** Localize every error in the list, returning `{field, message}` pairs the
 *  editor renders inline. */
export function localizeErrors(
  errs: ScenarioError[],
  locale: Locale,
): { field: string; message: string }[] {
  return errs.map((e) => ({ field: e.field, message: localizeError(e, locale) }))
}
