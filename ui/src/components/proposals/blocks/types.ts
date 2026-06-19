import type { ReactNode } from "react";

import type { ProposalFeedback } from "@/api/types";

export interface BlockProps {
  id: string;
  attributes: Record<string, string>;
  children?: ReactNode;
  /** Feedback entries anchored to this block (target_section === block.id). */
  feedback?: ProposalFeedback[];
}
