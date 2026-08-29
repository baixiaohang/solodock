import type { ApiErrorBody } from './types'

export class ApiError extends Error {
  constructor(public status: number, public body: ApiErrorBody) {
    super(body.message)
  }
}

let unauthorizedHandler: (() => void) | undefined

export function onUnauthorized(handler: () => void): void {
  unauthorizedHandler = handler
}

export function notifyUnauthorized(): void {
  unauthorizedHandler?.()
}

export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    ...init,
    credentials: 'same-origin',
    headers: { Accept: 'application/json', ...init.headers },
  })
  if (!response.ok) {
    const body = (await response.json()) as ApiErrorBody
    if (response.status === 401) unauthorizedHandler?.()
    throw new ApiError(response.status, body)
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

export async function mutation<T>(path: string, body?: unknown): Promise<T> {
  const csrf = readCookie('__Host-solodock_csrf')
  return api<T>(path, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...(csrf ? { 'X-CSRF-Token': csrf } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

export function readCookie(name: string): string | undefined {
  return document.cookie
    .split(';')
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${name}=`))
    ?.slice(name.length + 1)
}
