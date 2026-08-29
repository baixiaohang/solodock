import { writable } from 'svelte/store'
import { ApiError, api, mutation, onUnauthorized } from './api'
import type { MeResponse } from './types'

export type AuthState =
  | { kind: 'loading' }
  | { kind: 'setup' }
  | { kind: 'login' }
  | { kind: 'authenticated'; me: MeResponse }

export const auth = writable<AuthState>({ kind: 'loading' })
onUnauthorized(() => auth.set({ kind: 'login' }))

export async function loadSession(): Promise<void> {
  try {
    auth.set({ kind: 'authenticated', me: await api<MeResponse>('/api/v1/me') })
  } catch (error) {
    auth.set(resolveAuthError(error))
  }
}

export function resolveAuthError(error: unknown): AuthState {
  if (error instanceof ApiError && error.body.code === 'SETUP_REQUIRED') return { kind: 'setup' }
  return { kind: 'login' }
}

export async function bootstrap(token: string, password: string): Promise<void> {
  await api<void>('/api/v1/auth/bootstrap', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ bootstrap_token: token, password }),
  })
  auth.set({ kind: 'login' })
}

export async function login(password: string): Promise<void> {
  await api<void>('/api/v1/auth/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username: 'admin', password }),
  })
  await loadSession()
}

export async function logout(): Promise<void> {
  try { await mutation<void>('/api/v1/auth/logout') } finally { auth.set({ kind: 'login' }) }
}

export async function revokeAll(): Promise<void> {
  try { await mutation<void>('/api/v1/me/sessions/revoke-all') } finally { auth.set({ kind: 'login' }) }
}
