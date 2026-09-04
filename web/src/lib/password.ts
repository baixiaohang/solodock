import { localized, type LocalizedMessage } from './i18n'

export function validatePassword(value: string): LocalizedMessage | null {
  const trimmed = value.trim()
  const scalarCount = Array.from(trimmed).length
  const byteCount = new TextEncoder().encode(trimmed).length
  return scalarCount >= 14 && scalarCount <= 128 && byteCount <= 512
    ? null
    : localized('Password must contain 14–128 Unicode characters, excluding leading and trailing whitespace')
}
