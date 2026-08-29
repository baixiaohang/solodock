import { describe, expect, it } from 'vitest'
import { logicalRegistry } from './registryReference'

describe('registry reference projection', () => {
  it('matches Docker Hub aliases and explicit private registries', () => {
    expect(logicalRegistry('nginx:stable')).toBe('docker.io')
    expect(logicalRegistry('index.docker.io/library/nginx:stable')).toBe('docker.io')
    expect(logicalRegistry('ghcr.io/team/app:stable')).toBe('ghcr.io')
  })

  it('rejects values that cannot yet identify one logical registry', () => {
    expect(logicalRegistry('')).toBeNull()
    expect(logicalRegistry('https://ghcr.io/team/app:stable')).toBeNull()
    expect(logicalRegistry('ghcr.io/team/app@sha256:deadbeef')).toBeNull()
  })
})
