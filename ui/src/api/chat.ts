import { getServerBaseUrl } from "@/api/serverUrl";
import type { ChatMessage } from "@/stores/chatStore";

async function getBaseUrl(): Promise<string> {
  return getServerBaseUrl();
}

type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; media_type: string; data: string }
  | { type: "document"; media_type: string; data: string; filename?: string };

function messageToContent(
  message: ChatMessage
): string | ContentBlock[] {
  if (!message.attachments?.length) {
    return message.content;
  }
  const blocks: ContentBlock[] = [];
  for (const att of message.attachments) {
    if (att.mediaType.startsWith("image/")) {
      blocks.push({ type: "image", media_type: att.mediaType, data: att.data });
    } else {
      blocks.push({
        type: "document",
        media_type: att.mediaType,
        data: att.data,
        filename: att.filename,
      });
    }
  }
  if (message.content) {
    blocks.push({ type: "text", text: message.content });
  }
  return blocks;
}

/** Shape of a `tool_result` SSE event forwarded to the caller. */
export interface ChatToolResult {
  id: string;
  /**
   * Tool name. The server's `ToolResultPayload` does not include `name`, so it
   * is resolved from the prior `tool_call` event's id→name map. Falls back to
   * `undefined` when no matching call was seen on this stream.
   */
  name?: string;
  output: string;
  success: boolean;
  message?: string | null;
}

export interface SendChatMessageOptions {
  signal?: AbortSignal;
  systemPrompt?: string;
  onCompleteText?: (text: string) => void;
  /**
   * Fires once per session when the server emits its `session_title` event
   * (after the first assistant response). The server generates the title
   * now — the client no longer does a follow-up completion call.
   */
  onSessionTitle?: (title: string) => void;
  /**
   * Fires for every `tool_result` SSE event the server emits (one per tool
   * call). The payload is parsed from the server's `ToolResultPayload`
   * (`{id, output, elapsed_ms, success, message}`); `elapsed_ms` is dropped and
   * `name` is resolved from the prior `tool_call` event.
   */
  onToolResult?: (result: ChatToolResult) => void;
  /**
   * Scopes this chat to a proposal ("Address with djinn"). The server seeds the
   * system prompt with the proposal spec + unresolved feedback and grants the
   * proposal-editing tool subset. `feedbackId` highlights the entry the chat
   * was opened to address.
   */
  proposalId?: string;
  feedbackId?: string;
  proposalTargetSection?: string;
}

export async function sendChatMessage(
  sessionId: string,
  messages: ChatMessage[],
  model: string,
  _projectSlug: string | null,
  onDelta: (text: string) => void,
  onToolCall: (name: string, input?: unknown) => void,
  onDone: () => void,
  onError: (msg: string) => void,
  options?: SendChatMessageOptions
): Promise<void> {
  try {
    const baseUrl = await getBaseUrl();
    let completedText = "";

    // Chat is user-scoped, globally multi-project (chat-user-global refactor).
    // The request body no longer carries a `project` field — per-tool calls
    // specify their target project via the tool's `project` argument.
    const response = await fetch(`${baseUrl}/api/chat/completions`, {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        session_id: sessionId,
        system_prompt: options?.systemPrompt,
        messages: messages.map((message) => ({
          role: message.role,
          content: messageToContent(message),
        })),
        model,
        proposal_id: options?.proposalId,
        feedback_id: options?.feedbackId,
        proposal_target_section: options?.proposalTargetSection,
      }),
      signal: options?.signal,
    });

    if (!response.ok) {
      const message = `Chat request failed: ${response.status}`;
      onError(message);
      return;
    }

    if (!response.body) {
      onError("Chat response body is empty");
      return;
    }

    const decoder = new TextDecoder();
    const reader = response.body.getReader();
    let buffer = "";

    // tool_call events carry {name, id}; tool_result events only carry {id}.
    // Keep an id→name map so we can forward the resolved name alongside each
    // tool_result payload.
    const toolNameById = new Map<string, string>();

    const handleEvent = (chunk: string): void => {
      const trimmed = chunk.trim();
      if (!trimmed) return;

      let eventType = "message";
      const dataLines: string[] = [];

      for (const line of trimmed.split(/\r?\n/)) {
        if (line.startsWith("event:")) {
          eventType = line.slice("event:".length).trim();
          continue;
        }
        if (line.startsWith("data:")) {
          dataLines.push(line.slice("data:".length).trim());
        }
      }

      if (dataLines.length === 0) return;

      const dataText = dataLines.join("\n");
      let payload: Record<string, unknown> = {};

      try {
        payload = JSON.parse(dataText) as Record<string, unknown>;
      } catch {
        payload = { text: dataText, message: dataText, name: dataText };
      }

      switch (eventType) {
        case "delta": {
          const text = typeof payload.text === "string" ? payload.text : "";
          if (text) {
            onDelta(text);
            completedText += text;
          }
          break;
        }
        case "tool_call": {
          const name = typeof payload.name === "string" ? payload.name : "tool";
          const input = "input" in payload ? payload.input : undefined;
          if (typeof payload.id === "string") {
            toolNameById.set(payload.id, name);
          }
          onToolCall(name, input);
          break;
        }
        case "tool_result": {
          if (options?.onToolResult) {
            const id = typeof payload.id === "string" ? payload.id : "";
            const output = typeof payload.output === "string" ? payload.output : "";
            const success = payload.success !== false;
            const message =
              typeof payload.message === "string" ? payload.message : null;
            const name = id ? toolNameById.get(id) : undefined;
            options.onToolResult({ id, name, output, success, message });
          }
          break;
        }
        case "session_title": {
          const title = typeof payload.title === "string" ? payload.title : "";
          if (title && options?.onSessionTitle) {
            options.onSessionTitle(title);
          }
          break;
        }
        case "done":
          onDone();
          break;
        case "error": {
          const message =
            typeof payload.message === "string"
              ? payload.message
              : "Unknown chat stream error";
          onError(message);
          break;
        }
        default:
          break;
      }
    };

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const events = buffer.split("\n\n");
      buffer = events.pop() ?? "";

      for (const eventChunk of events) {
        handleEvent(eventChunk);
      }
    }

    if (buffer.trim()) {
      handleEvent(buffer);
    }

    if (options?.onCompleteText) {
      options.onCompleteText(completedText);
    }
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") {
      return;
    }
    const message = error instanceof Error ? error.message : "Chat request failed";
    onError(message);
  }
}
