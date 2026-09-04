import type { ApiErrorBody } from './types'

const MAX_REQUEST_ID_LENGTH = 128
const REQUEST_ID_PATTERN = /^[A-Za-z0-9._:-]+$/

function safeRequestId(value: unknown): string | undefined {
  if (typeof value !== 'string') return undefined
  const trimmed = value.trim()
  return trimmed.length > 0
    && trimmed.length <= MAX_REQUEST_ID_LENGTH
    && REQUEST_ID_PATTERN.test(trimmed)
    ? trimmed
    : undefined
}

function isJsonContentType(value: string | null): boolean {
  if (!value) return false
  const mediaType = value.split(';', 1)[0].trim().toLowerCase()
  return mediaType === 'application/json' || mediaType.endsWith('+json')
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function validIssues(value: unknown): value is NonNullable<ApiErrorBody['issues']> {
  return Array.isArray(value) && value.every((issue) =>
    isRecord(issue)
    && typeof issue.path === 'string'
    && typeof issue.code === 'string'
    && typeof issue.message === 'string')
}

export type MutationOutcome = 'known_not_applied' | 'outcome_unknown'

interface NormalizedErrorResponse {
  body: ApiErrorBody
  trustedBackendEnvelope: boolean
}

function fallbackError(status: number, requestId: string): ApiErrorBody {
  return {
    code: 'HTTP_ERROR',
    message: `Request failed with HTTP status ${status}`,
    request_id: requestId,
  }
}

async function normalizeErrorResponse(response: Response): Promise<NormalizedErrorResponse> {
  const headerRequestId = safeRequestId(response.headers.get('X-Request-ID'))
  const fallback = fallbackError(response.status, headerRequestId ?? '')
  if (!isJsonContentType(response.headers.get('Content-Type'))) {
    return { body: fallback, trustedBackendEnvelope: false }
  }

  let value: unknown
  try {
    value = await response.json()
  } catch {
    return { body: fallback, trustedBackendEnvelope: false }
  }
  if (!isRecord(value)
    || typeof value.code !== 'string'
    || value.code.length === 0
    || typeof value.message !== 'string'
    || value.message.length === 0
    || (value.request_id !== undefined && safeRequestId(value.request_id) === undefined)
    || (value.issues !== undefined && !validIssues(value.issues))) {
    return { body: fallback, trustedBackendEnvelope: false }
  }

  return {
    body: {
      code: value.code,
      message: value.message,
      request_id: headerRequestId ?? safeRequestId(value.request_id) ?? '',
      ...(value.issues === undefined ? {} : { issues: value.issues }),
    },
    trustedBackendEnvelope: true,
  }
}

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: ApiErrorBody,
    public readonly mutationOutcome: MutationOutcome = 'outcome_unknown',
  ) {
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

export async function api<T>(
  path: string,
  init: RequestInit = {},
  options: { expectedStatus?: number } = {},
): Promise<T> {
  const response = await fetch(path, {
    ...init,
    credentials: 'same-origin',
    headers: { Accept: 'application/json', ...init.headers },
  })
  if (!response.ok) {
    if (response.status === 401) unauthorizedHandler?.()
    const normalized = await normalizeErrorResponse(response)
    throw new ApiError(
      response.status,
      normalized.body,
      normalized.trustedBackendEnvelope && response.status >= 400 && response.status < 500
        ? 'known_not_applied'
        : 'outcome_unknown',
    )
  }
  if (options.expectedStatus !== undefined && response.status !== options.expectedStatus) {
    throw new ApiError(response.status, {
      code: 'HTTP_ERROR',
      message: `Unexpected HTTP status ${response.status}`,
      request_id: safeRequestId(response.headers.get('X-Request-ID')) ?? '',
    })
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

export async function mutation<T>(
  path: string,
  body?: unknown,
  options: { method?: 'POST' | 'PUT' | 'DELETE'; idempotencyKey?: string; expectedStatus?: number } = {},
): Promise<T> {
  const csrf = readCookie('__Host-solodock_csrf')
  return api<T>(path, {
    method: options.method ?? 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...(csrf ? { 'X-CSRF-Token': csrf } : {}),
      ...(options.idempotencyKey ? { 'Idempotency-Key': options.idempotencyKey } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  }, { expectedStatus: options.expectedStatus })
}

export function readCookie(name: string): string | undefined {
  return document.cookie
    .split(';')
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${name}=`))
    ?.slice(name.length + 1)
}
