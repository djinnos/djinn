const PROPOSAL_TOUR_VERSION = "v1";

export function proposalDetailTourStorageKey(userId: string): string {
  return `djinn:proposal-detail-tour:${PROPOSAL_TOUR_VERSION}:${userId}`;
}
