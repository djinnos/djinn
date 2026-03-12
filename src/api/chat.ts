import { getServerPort } from "@/tauri/commands";
import type { ChatMessage } from "@/stores/chatStore";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

export interface SendChatMessageOptions {
  signal?: AbortSignal;
  systemPrompt?: string;
  onCompleteText?: (text: string) => void;
}

export async function sendChatMessage(
  messages: ChatMessage[],
  model: string,
  projectPath: string | null,
  onDelta: (text: string) => void,
  onToolCall: (name: string) => void,
  onDone: () => void,
  onError: (msg: string) => void,
  options?: SendChatMessageOptions
): Promise<void> {
  try {
    const baseUrl = await getBaseUrl();
    let completedText = "";

    const response = await fetch(`${baseUrl}/api/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        system_prompt: options?.systemPrompt,
        messages: messages.map((message) => ({
          role: message.role,
          content: message.content,
        })),
        model,
        project_path: projectPath,
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
          onToolCall(name);
          break;
        }
        case "tool_result":
          break;
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
