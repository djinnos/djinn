import { useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { useChatStore } from "@/stores/chatStore";
import type { Proposal, ProposalFeedback } from "@/api/types";

function newSessionId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * Open a proposal-scoped chat ("Address with djinn"): mint a session, attach the
 * proposal (and optional feedback) scope so the server seeds the spec context
 * and grants the proposal-editing tools, then navigate to the chat. Pass no
 * feedback for a general "ask djinn about this proposal" chat.
 */
export function useStartProposalChat() {
  const navigate = useNavigate();
  const upsertSession = useChatStore((s) => s.upsertSession);
  const setSessionScope = useChatStore((s) => s.setSessionScope);
  const setActiveSession = useChatStore((s) => s.setActiveSession);

  return useCallback(
    (
      proposal: Proposal,
      feedback?: ProposalFeedback,
      feedbackAuthor?: string,
      targetSection?: string,
    ) => {
      const sessionId = newSessionId();
      const now = Date.now();
      upsertSession({
        id: sessionId,
        title: `Address: ${proposal.title}`,
        projectSlug: null,
        model: null,
        createdAt: now,
        updatedAt: now,
      });
      setSessionScope(sessionId, {
        proposalId: proposal.id,
        proposalShortId: proposal.short_id,
        proposalTitle: proposal.title,
        targetSection,
        feedbackId: feedback?.id,
        feedbackAuthor,
        feedbackBody: feedback?.body,
      });
      setActiveSession(sessionId);
      navigate("/chat");
    },
    [navigate, upsertSession, setSessionScope, setActiveSession]
  );
}
