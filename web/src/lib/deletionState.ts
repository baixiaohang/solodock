import type { DeletionPreviewResponse } from './types'

export function canConfirmDeletion(
  preview: DeletionPreviewResponse | null,
  typedSlug: string,
  selectedRemoveContainer: boolean,
  now = Date.now(),
): boolean {
  return preview !== null
    && preview.slug === typedSlug
    && preview.remove_container === selectedRemoveContainer
    && Date.parse(preview.expires_at) > now
}
