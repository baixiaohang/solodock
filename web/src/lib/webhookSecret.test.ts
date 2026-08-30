import { describe, expect, it } from 'vitest'
import { encodeWebhookSecret } from './webhookSecret'

describe('webhook secret handling', () => {
  it('encodes exactly 32 random bytes and clears the source buffer', () => {
    const bytes = Uint8Array.from({ length: 32 }, (_, index) => index)
    expect(encodeWebhookSecret(bytes)).toBe('AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8')
    expect([...bytes]).toEqual(Array(32).fill(0))
  })

  it('rejects weak-sized input', () => {
    expect(() => encodeWebhookSecret(new Uint8Array(16))).toThrow()
  })
})

