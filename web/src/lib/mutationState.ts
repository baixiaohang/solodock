export interface RetryIdentity { fingerprint: string; key: string }

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
