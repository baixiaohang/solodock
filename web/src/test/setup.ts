import { beforeEach } from 'vitest'
import { setLocale } from '../lib/i18n'

beforeEach(() => {
  setLocale('zh-CN', false)
})
