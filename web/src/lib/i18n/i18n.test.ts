// @vitest-environment jsdom
import { get } from 'svelte/store'
import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  LOCALE_STORAGE_KEY,
  dictionaryKeys,
  locale,
  resolveInitialLocale,
  setLocale,
  translateForLocale,
} from '.'

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
})

describe('UI locale selection', () => {
  it('uses a valid explicit selection before the browser language', () => {
    const storage = { getItem: () => 'en' }
    expect(resolveInitialLocale(storage, ['zh-CN'])).toBe('en')
  })

  it('maps only the first zh browser language to zh-CN and otherwise defaults to English', () => {
    expect(resolveInitialLocale(undefined, ['zh'])).toBe('zh-CN')
    expect(resolveInitialLocale(undefined, ['zh-Hant'])).toBe('zh-CN')
    expect(resolveInitialLocale(undefined, ['fr-FR', 'zh-CN'])).toBe('en')
    expect(resolveInitialLocale(undefined, undefined)).toBe('en')
  })

  it('ignores invalid values and storage read failures', () => {
    expect(resolveInitialLocale({ getItem: () => 'fr' }, ['zh-CN'])).toBe('zh-CN')
    expect(resolveInitialLocale({ getItem: () => { throw new Error('blocked') } }, ['en-US'])).toBe('en')
  })

  it('falls back when obtaining the browser storage object throws', () => {
    const descriptor = Object.getOwnPropertyDescriptor(window, 'localStorage')
    vi.stubGlobal('navigator', { languages: ['zh-CN'], language: 'zh-CN' })
    Object.defineProperty(window, 'localStorage', {
      configurable: true,
      get: () => { throw new DOMException('blocked', 'SecurityError') },
    })
    try {
      expect(resolveInitialLocale()).toBe('zh-CN')
    } finally {
      if (descriptor) Object.defineProperty(window, 'localStorage', descriptor)
    }
  })

  it('persists a selection for refresh and updates the html lang immediately', () => {
    setLocale('en')
    expect(localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('en')
    expect(resolveInitialLocale(localStorage, ['zh-CN'])).toBe('en')
    expect(document.documentElement.lang).toBe('en')
  })

  it('keeps the current-page selection when storage writes fail', () => {
    vi.stubGlobal('localStorage', { setItem: () => { throw new Error('blocked') } })
    setLocale('en')
    expect(get(locale)).toBe('en')
    expect(document.documentElement.lang).toBe('en')
  })

  it('keeps English and Chinese dictionary keys in parity', () => {
    expect(dictionaryKeys['zh-CN'].sort()).toEqual(dictionaryKeys.en.sort())
    expect(translateForLocale('en', 'Applications')).toBe('Applications')
    expect(translateForLocale('zh-CN', 'Applications')).toBe('应用')
  })
})
