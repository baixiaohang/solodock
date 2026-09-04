import { ApiError, type MutationOutcome } from './api'

export interface RetryIdentity { fingerprint: string; key: string }

export class LocalMutationValidationError extends Error {}

export interface MutationFailure<T> {
  outcome: MutationOutcome
  retry: T | undefined
}

export function mutationFailure<T>(retry: T | undefined, cause: unknown): MutationFailure<T> {
  const outcome: MutationOutcome = cause instanceof LocalMutationValidationError
    ? 'known_not_applied'
    : cause instanceof ApiError
      ? cause.mutationOutcome
      : 'outcome_unknown'
  return {
    outcome,
    retry: outcome === 'outcome_unknown' ? retry : undefined,
  }
}

export function retryIdentity(
  previous: RetryIdentity | undefined,
  body: unknown,
  randomKey: () => string = () => crypto.randomUUID(),
): RetryIdentity {
  const fingerprint = JSON.stringify(body)
  return previous?.fingerprint === fingerprint
    ? previous
    : { fingerprint, key: randomKey() }
}

export interface DeletionChoice { removeContainer: boolean; previewLocked: boolean }

export const initialDeletionChoice = (): DeletionChoice => ({
  removeContainer: false,
  previewLocked: false,
})
