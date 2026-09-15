import type { Translate } from './i18n'

const localizedDescriptions: Record<string, Parameters<Translate>[0]> = {
  pgadmin: 'pgAdmin with persistent settings and internal access to PostgreSQL services.',
  postgresql: 'Single-instance PostgreSQL with a persistent volume and the platform service-discovery network.',
}

export function presetDescription(id: string, canonicalDescription: string, translate: Translate): string {
  const key = localizedDescriptions[id]
  return key ? translate(key) : canonicalDescription
}
