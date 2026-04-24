/**
 * Client for the DB-backed chat sessions API.
 *
 * The server owns chat persistence — this module provides typed wrappers
 * around `/api/chat/sessions(/:id/...)`. All requests include credentials so
 * the session cookie is sent.
 */
import { getServerBaseUrl } from '@/api/serverUrl';
import type { ChatAttachment, ChatMessage, ChatSession } from '@/stores/chatStore';

interface ServerChatSession {
  id: string;
  title: string;
  project_slug?: string | null;
  model?: string | null;
  created_at: number;
  updated_at: number;
}

type ServerContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; media_type: string; data: string }
  | { type: 'document'; media_type: string; data: string; filename?: string };

interface ServerChatMessage {
  id: string;
  role: 'user' | 'assistant';
  content: string | ServerContentBlock[];
  attachments?: ChatAttachment[];
  tool_calls?: { name: string; success?: boolean; input?: unknown }[];
  created_at: number;
}

function mapSession(raw: ServerChatSession): ChatSession {
  return {
    id: raw.id,
    title: raw.title,
    projectSlug: raw.project_slug ?? null,
    model: raw.model ?? null,
    createdAt: raw.created_at,
    updatedAt: raw.updated_at,
  };
}

function contentBlocksToText(blocks: ServerContentBlock[]): {
  text: string;
  attachments?: ChatAttachment[];
} {
  const textParts: string[] = [];
  const attachments: ChatAttachment[] = [];
  for (const block of blocks) {
    if (block.type === 'text') {
      textParts.push(block.text);
    } else if (block.type === 'image') {
      attachments.push({
        id: `${attachments.length}`,
        filename: 'image',
        mediaType: block.media_type,
        data: block.data,
        url: `data:${block.media_type};base64,${block.data}`,
      });
    } else if (block.type === 'document') {
      attachments.push({
        id: `${attachments.length}`,
        filename: block.filename ?? 'document',
        mediaType: block.media_type,
        data: block.data,
      });
    }
  }
  return {
    text: textParts.join('\n'),
    attachments: attachments.length > 0 ? attachments : undefined,
  };
}

function mapMessage(raw: ServerChatMessage): ChatMessage {
  let content: string;
  let attachments: ChatAttachment[] | undefined;
  if (typeof raw.content === 'string') {
    content = raw.content;
    attachments = raw.attachments;
  } else {
    const parsed = contentBlocksToText(raw.content);
    content = parsed.text;
    attachments = parsed.attachments ?? raw.attachments;
  }
  return {
    id: raw.id,
    role: raw.role,
    content,
    attachments,
    toolCalls: raw.tool_calls,
    createdAt: raw.created_at,
  };
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${getServerBaseUrl()}${path}`, {
    credentials: 'include',
    headers: {
      Accept: 'application/json',
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...(init?.headers ?? {}),
    },
    ...init,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`${init?.method ?? 'GET'} ${path} failed: ${res.status}${text ? ` — ${text}` : ''}`);
  }
  // DELETE can return 204 without a body.
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export async function listChatSessions(): Promise<ChatSession[]> {
  const body = await request<{ sessions: ServerChatSession[] }>('/api/chat/sessions');
  return body.sessions.map(mapSession);
}

export async function getChatSessionMessages(sessionId: string): Promise<ChatMessage[]> {
  const body = await request<{ messages: ServerChatMessage[] }>(
    `/api/chat/sessions/${encodeURIComponent(sessionId)}/messages`,
  );
  return body.messages.map(mapMessage);
}

export async function deleteChatSession(sessionId: string): Promise<void> {
  await request<void>(`/api/chat/sessions/${encodeURIComponent(sessionId)}`, {
    method: 'DELETE',
  });
}

export async function renameChatSession(sessionId: string, title: string): Promise<void> {
  await request<void>(`/api/chat/sessions/${encodeURIComponent(sessionId)}`, {
    method: 'PATCH',
    body: JSON.stringify({ title }),
  });
}
