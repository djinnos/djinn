import { Client } from "@modelcontextprotocol/sdk/client";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
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
}

export async function callMcpTool<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  const invoke = async (reconnect: boolean): Promise<T> => {
    const client = await connectClient(reconnect);
    const result = (await client.callTool({ name, arguments: args ?? {} })) as ToolCallResult;
    if (result.isError) {
      throw new Error(buildErrorMessage(result));
    }
    return extractToolPayload<T>(result);
  };

  try {
    return await invoke(false);
  } catch (error) {
    if (!(error instanceof Error)) {
      throw error;
    }
    return invoke(true);
  }
}
