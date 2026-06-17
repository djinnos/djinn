import { beforeEach, describe, expect, it, vi } from "vitest";

import { getServerBaseUrl } from "@/api/serverUrl";
import { sendChatMessage, type ChatToolResult } from "./chat";

vi.mock("@/api/serverUrl", () => ({
  getServerBaseUrl: vi.fn(),
}));

const getServerBaseUrlMock = vi.mocked(getServerBaseUrl);

/**
 * Encode an array of SSE event frames (already including trailing `\n\n`)
 * into a single UTF-8 Uint8Array. Mirrors how the server writes a
 * `text/event-stream` body.
 */
function encodeSse(frames: string[]): Uint8Array {
  const encoder = new TextEncoder();
  const chunks = frames.map((frame) => encoder.encode(frame));
  const total = chunks.reduce((sum, c) => sum + c.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

function sseFrame(event: string, data: unknown): string {
  const dataText = typeof data === "string" ? data : JSON.stringify(data);
  return `event:${event}\ndata:${dataText}\n\n`;
}

/**
 * Build a fetch Response whose body is a ReadableStream emitting the given
 * SSE frames. `sendChatMessage` reads via `getReader()` + TextDecoder.
 */
function streamingResponse(frames: string[]): Response {
  const bytes = encodeSse(frames);
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
  return new Response(stream, {
    status: 200,
    headers: { "Content-Type": "text/event-stream" },
  });
}

beforeEach(() => {
  vi.restoreAllMocks();
  getServerBaseUrlMock.mockReset();
  getServerBaseUrlMock.mockReturnValue("https://djinn.example.test");
});

describe("sendChatMessage tool_result handling", () => {
  it("invokes onToolResult with the parsed payload for each tool_result event", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        streamingResponse([
          sseFrame("tool_call", { name: "code_graph", id: "call_1", input: { op: "impact" } }),
          sseFrame("tool_result", {
            id: "call_1",
            output: '{"nodes":[]}',
            elapsed_ms: 12,
            success: true,
          }),
          sseFrame("done", {}),
        ]),
      );
    vi.stubGlobal("fetch", fetchMock);

    const results: ChatToolResult[] = [];
    await sendChatMessage(
      "session-1",
      [],
      "model",
      null,
      () => {},
      () => {},
      () => {},
      () => {},
      {
        onToolResult: (r) => results.push(r),
      },
    );

    expect(results).toHaveLength(1);
    expect(results[0]).toMatchObject({
      id: "call_1",
      name: "code_graph",
      output: '{"nodes":[]}',
      success: true,
      message: null,
    });
  });

  it("resolves the tool name from the prior tool_call event", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        streamingResponse([
          sseFrame("tool_call", { name: "read_file", id: "abc", input: { path: "a.rs" } }),
          sseFrame("tool_call", { name: "search", id: "def", input: { q: "foo" } }),
          sseFrame("tool_result", { id: "def", output: "no matches", elapsed_ms: 1, success: true }),
          sseFrame("tool_result", { id: "abc", output: "fn main() {}", elapsed_ms: 2, success: true }),
          sseFrame("done", {}),
        ]),
      );
    vi.stubGlobal("fetch", fetchMock);

    const results: ChatToolResult[] = [];
    await sendChatMessage(
      "session-1",
      [],
      "model",
      null,
      () => {},
      () => {},
      () => {},
      () => {},
      { onToolResult: (r) => results.push(r) },
    );

    expect(results).toHaveLength(2);
    expect(results[0]).toMatchObject({ id: "def", name: "search" });
    expect(results[1]).toMatchObject({ id: "abc", name: "read_file" });
  });

  it("leaves name undefined when no matching tool_call was seen", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        streamingResponse([
          sseFrame("tool_result", { id: "orphan", output: "x", elapsed_ms: 0, success: false }),
          sseFrame("done", {}),
        ]),
      );
    vi.stubGlobal("fetch", fetchMock);

    const results: ChatToolResult[] = [];
    await sendChatMessage(
      "session-1",
      [],
      "model",
      null,
      () => {},
      () => {},
      () => {},
      () => {},
      { onToolResult: (r) => results.push(r) },
    );

    expect(results).toHaveLength(1);
    expect(results[0].name).toBeUndefined();
    expect(results[0].success).toBe(false);
  });

  it("does not fire onToolResult when the callback is not provided", async () => {
    // Ensures the tool_result branch is a safe no-op when no callback is wired,
    // matching the original behavior before this task.
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        streamingResponse([
          sseFrame("tool_result", { id: "call_1", output: "ok", elapsed_ms: 1, success: true }),
          sseFrame("done", {}),
        ]),
      );
    vi.stubGlobal("fetch", fetchMock);

    // No options → no onToolResult. Should resolve without throwing.
    await expect(
      sendChatMessage("session-1", [], "model", null, () => {}, () => {}, () => {}, () => {}),
    ).resolves.toBeUndefined();
  });
});
