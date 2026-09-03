import { derived, get, writable } from 'svelte/store'
import { en, type MessageKey, type Messages } from './en'
import { zhCN } from './zh-CN'

export type { MessageKey } from './en'

export type Locale = 'en' | 'zh-CN'
export type Interpolation = Record<string, string | number>
export type Translate = (key: MessageKey, values?: Interpolation) => string
export interface LocalizedMessage {
  key: MessageKey
  values?: Interpolation
}
export type UserMessage = string | LocalizedMessage

export const LOCALE_STORAGE_KEY = 'solodock.ui.locale.v1'
const dictionaries: Record<Locale, Messages> = { en, 'zh-CN': zhCN }
export const dictionaryKeys: Record<Locale, string[]> = {
  en: Object.keys(en),
  'zh-CN': Object.keys(zhCN),
}

export function browserLocale(languages: readonly string[] | undefined): Locale {
  const language = languages?.[0]?.toLowerCase()
  return language === 'zh' || language?.startsWith('zh-') ? 'zh-CN' : 'en'
}

function defaultStorage(): Pick<Storage, 'getItem'> | undefined {
  try {
    return typeof localStorage === 'undefined' ? undefined : localStorage
  } catch {
    return undefined
  }
}

function defaultLanguages(): readonly string[] | undefined {
  try {
    if (typeof navigator === 'undefined') return undefined
    return navigator.languages.length > 0
      ? navigator.languages
      : navigator.language
        ? [navigator.language]
        : undefined
  } catch {
    return undefined
  }
}

export function resolveInitialLocale(
  storage: Pick<Storage, 'getItem'> | undefined = defaultStorage(),
  languages: readonly string[] | undefined = defaultLanguages(),
): Locale {
  try {
    const stored = storage?.getItem(LOCALE_STORAGE_KEY)
    if (stored === 'en' || stored === 'zh-CN') return stored
  } catch {
    // Browser privacy settings may make storage unavailable.
  }
  return browserLocale(languages)
}

function format(message: string, values: Interpolation = {}): string {
  return message.replace(/\{([A-Za-z][A-Za-z0-9]*)\}/g, (placeholder, name: string) =>
    Object.hasOwn(values, name) ? String(values[name]) : placeholder)
}

export function translateForLocale(locale: Locale, key: MessageKey, values?: Interpolation): string {
  return format(dictionaries[locale][key], values)
}

export const locale = writable<Locale>(resolveInitialLocale())
export const t = derived(locale, ($locale): Translate =>
  (key, values) => translateForLocale($locale, key, values))

export function currentLocale(): Locale {
  return get(locale)
}

export function translate(key: MessageKey, values?: Interpolation): string {
  return translateForLocale(currentLocale(), key, values)
}

export function localized(key: MessageKey, values?: Interpolation): LocalizedMessage {
  return values ? { key, values } : { key }
}

export function messageText(message: UserMessage, translateMessage: Translate = translate): string {
  return typeof message === 'string' ? message : translateMessage(message.key, message.values)
}

export function setLocale(next: Locale, persist = true): void {
  if (persist) {
    try { localStorage.setItem(LOCALE_STORAGE_KEY, next) } catch {
      // Keep the current-page selection even when storage is unavailable.
    }
  }
  locale.set(next)
}

locale.subscribe((next) => {
  if (typeof document !== 'undefined') document.documentElement.lang = next
})
