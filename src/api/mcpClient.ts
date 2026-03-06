import { Client } from "@modelcontextprotocol/sdk/client";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { McpToolInput, McpToolName, McpToolOutput } from "@/api/generated/mcp-tools.gen";
import { getServerPort } from "@/tauri/commands";

type ToolCallResult = {
  content?: Array<{ type?: string; text?: string }>;
  structuredContent?: unknown;
  isError?: boolean;
  toolResult?: unknown;
};

let activeClient: Client | null = null;
let activeTransport: StreamableHTTPClientTransport | null = null;
let activeUrl: string | null = null;
let connectPromise: Promise<Client> | null = null;

function buildErrorMessage(result: ToolCallResult): string {
  if (!result.content) return "MCP tool call failed";

  const firstText = result.content.find((item) => item.type === "text" && typeof item.text === "string")?.text;
  if (!firstText) return "MCP tool call failed";

  try {
    const parsed = JSON.parse(firstText) as { error?: string; message?: string };
    return parsed.error ?? parsed.message ?? firstText;
  } catch {
    return firstText;
  }
}

function extractToolPayload<T>(result: ToolCallResult): T {
  if (result.structuredContent !== undefined) {
    return result.structuredContent as T;
  }

  if (result.toolResult !== undefined) {
    return result.toolResult as T;
  }

  const firstText = result.content?.find((item) => item.type === "text" && typeof item.text === "string")?.text;
  if (firstText) {
    try {
      return JSON.parse(firstText) as T;
    } catch {
      throw new Error("MCP tool returned non-JSON text response");
    }
  }

  throw new Error("MCP tool returned no structured payload");
}

async function getMcpUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}/mcp`;
}

async function connectClient(forceReconnect = false): Promise<Client> {
  const url = await getMcpUrl();

  if (!forceReconnect && activeClient && activeUrl === url) {
    return activeClient;
  }

  // Serialize concurrent connection attempts — if one is already in-flight, wait for it
  if (!forceReconnect && connectPromise) {
    return connectPromise;
  }

  const doConnect = async (): Promise<Client> => {
    if (activeTransport) {
      await activeTransport.close().catch(() => undefined);
    }

    const client = new Client({ name: "djinn-desktop", version: "0.1.0" });
    const transport = new StreamableHTTPClientTransport(new URL(url));
    await client.connect(transport);

    activeClient = client;
    activeTransport = transport;
    activeUrl = url;
    return client;
  };

  connectPromise = doConnect().finally(() => {
    connectPromise = null;
  });

  return connectPromise;
}

/**
 * Reset the cached MCP client so the next call reconnects.
 * Called when the server restarts on a new port.
 */
export async function resetMcpClient(): Promise<void> {
  if (activeTransport) {
    await activeTransport.close().catch(() => undefined);
  }
  activeClient = null;
  activeTransport = null;
  activeUrl = null;
  connectPromise = null;
}

const MAX_RETRIES = 3;
const INITIAL_BACKOFF_MS = 500;
const REQUEST_TIMEOUT_MS = 30_000;

function isTimeoutError(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  return error.message.includes("timed out") || error.message.includes("-32001");
}

export async function callMcpTool<TName extends McpToolName>(
  name: TName,
  args?: McpToolInput<TName>
): Promise<McpToolOutput<TName>> {
  const invoke = async (reconnect: boolean): Promise<McpToolOutput<TName>> => {
    const client = await connectClient(reconnect);
    const result = (await client.callTool({
      name,
      arguments: (args ?? {}) as Record<string, unknown>,
    }, undefined, { timeout: REQUEST_TIMEOUT_MS })) as ToolCallResult;
    if (result.isError) {
      throw new Error(buildErrorMessage(result));
    }
    return extractToolPayload<McpToolOutput<TName>>(result);
  };

  let lastError: unknown;
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    try {
      return await invoke(attempt > 0);
    } catch (error) {
      lastError = error;
      if (!isTimeoutError(error) || attempt === MAX_RETRIES) break;
      await new Promise((r) => setTimeout(r, INITIAL_BACKOFF_MS * 2 ** attempt));
    }
  }
  throw lastError;
}
