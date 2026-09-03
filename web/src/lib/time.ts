import { writable } from 'svelte/store'
import { api } from './api'
import type { SettingsResponse } from './types'
import { currentLocale, type Locale } from './i18n'

export interface TimeDisplayState {
  configuredTimezone: string
  timezone: string
  unsupportedTimezone: string | null
}

export const timeSettings = writable<TimeDisplayState>({
  configuredTimezone: 'UTC',
  timezone: 'UTC',
  unsupportedTimezone: null,
})

export async function loadTimeSettings(): Promise<SettingsResponse> {
  const settings = await api<SettingsResponse>('/api/v1/settings')
  applyTimeSettings(settings)
  return settings
}

export function applyTimeSettings(settings: Pick<SettingsResponse, 'display_timezone'>): void {
  const supported = browserSupportsTimezone(settings.display_timezone)
  timeSettings.set({
    configuredTimezone: settings.display_timezone,
    timezone: supported ? settings.display_timezone : 'UTC',
    unsupportedTimezone: supported ? null : settings.display_timezone,
  })
}

export function browserSupportsTimezone(timezone: string): boolean {
  try {
    new Intl.DateTimeFormat('en', { timeZone: timezone }).format(new Date(0))
    return true
  } catch {
    return false
  }
}

export function formatTimestamp(value: string | null | undefined, timezone: string, locale: Locale = currentLocale()): string {
  if (!value) return '—'
  const parsed = new Date(value)
  if (Number.isNaN(parsed.valueOf())) return '—'
  try {
    return new Intl.DateTimeFormat(locale, {
      timeZone: timezone,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hourCycle: 'h23',
    }).format(parsed)
  } catch {
    return new Intl.DateTimeFormat(locale, {
      timeZone: 'UTC',
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hourCycle: 'h23',
    }).format(parsed)
  }
}
