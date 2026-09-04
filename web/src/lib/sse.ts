import type { LogEvent } from './types'
import { notifyUnauthorized } from './api'

export function appendBounded<T>(items: T[], item: T, limit: number): T[] {
  const next = [...items, item]
  return next.length > limit ? next.slice(next.length - limit) : next
}

export function appendDeduplicatedLog(
  items: Array<LogEvent & { id: string }>,
  item: LogEvent & { id: string },
): Array<LogEvent & { id: string }> {
  if (items.some((current) => current.id === item.id)) return items
  return appendBounded(items, item, 1000)
}

export function openSse(path: string, handlers: Record<string, (event: MessageEvent<string>) => void>): EventSource {
  const source = new EventSource(path, { withCredentials: true })
  try {
    for (const [name, handler] of Object.entries(handlers)) source.addEventListener(name, handler as EventListener)
    source.addEventListener('stream_error', ((event: MessageEvent<string>) => {
      try {
        if ((JSON.parse(event.data) as { code?: string }).code === 'SESSION_EXPIRED') notifyUnauthorized()
      } catch {
        // Stable server events are JSON; malformed events do not reach application state.
      }
    }) as EventListener)
    return source
  } catch (cause) {
    source.close()
    throw cause
  }
}
