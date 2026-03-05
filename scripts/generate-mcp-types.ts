import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { compile } from "json-schema-to-typescript";
import { Client } from "@modelcontextprotocol/sdk/client";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";

type JsonSchema = Record<string, unknown>;

type ToolSchema = {
  name: string;
  inputSchema?: JsonSchema;
  outputSchema?: JsonSchema;
  input_schema?: JsonSchema;
  output_schema?: JsonSchema;
};

type ToolListResult = {
  tools: ToolSchema[];
};

type Options = {
  source: "snapshot" | "live";
  outFile: string;
  snapshotFile: string;
  mcpUrl: string;
};

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const desktopRoot = path.resolve(__dirname, "..");

function parseArgs(): Options {
  const args = process.argv.slice(2);
  const get = (name: string): string | undefined => {
    const match = args.find((arg) => arg.startsWith(`${name}=`));
    return match ? match.slice(name.length + 1) : undefined;
  };

  const sourceArg = get("--source") ?? "snapshot";
  const source = sourceArg === "live" ? "live" : "snapshot";

  const outFile = path.resolve(
    desktopRoot,
    get("--out") ?? "src/api/generated/mcp-tools.gen.ts"
  );
  const snapshotFile = path.resolve(
    desktopRoot,
    get("--snapshot") ?? "../server/tests/fixtures/mcp_tools_schema_snapshot.json"
  );
  const mcpUrl = get("--url") ?? process.env.DJINN_MCP_URL ?? "http://127.0.0.1:3000/mcp";

  return { source, outFile, snapshotFile, mcpUrl };
}

function toPascalCase(input: string): string {
  return input
    .split(/[^a-zA-Z0-9]+/)
    .filter(Boolean)
    .map((part) => part[0].toUpperCase() + part.slice(1))
    .join("");
}

function normalizeSchema(raw: JsonSchema | undefined): JsonSchema {
  if (!raw) {
    return { type: "object", additionalProperties: true };
  }

  const replacer = (value: unknown): unknown => {
    if (Array.isArray(value)) {
      return value.map(replacer);
    }

    if (value && typeof value === "object") {
      const record = value as Record<string, unknown>;
      const next: Record<string, unknown> = {};

      for (const [key, current] of Object.entries(record)) {
        if (key === "$schema") {
          continue;
        }

        if (key === "title") {
          continue;
        }

        if (key === "format" && (current === "uint" || current === "uint32" || current === "uint64")) {
          continue;
        }

        next[key] = replacer(current);
      }

      return next;
    }

    return value;
  };

  return replacer(raw) as JsonSchema;
}

async function loadToolsFromSnapshot(snapshotFile: string): Promise<ToolSchema[]> {
  const raw = await readFile(snapshotFile, "utf8");
  const parsed = JSON.parse(raw) as ToolListResult;
  return parsed.tools ?? [];
}

async function loadToolsFromLive(mcpUrl: string): Promise<ToolSchema[]> {
  const client = new Client({ name: "djinn-mcp-typegen", version: "0.1.0" });
  const transport = new StreamableHTTPClientTransport(new URL(mcpUrl));
  await client.connect(transport);
  try {
    const listed = (await client.listTools()) as ToolListResult;
    return listed.tools ?? [];
  } finally {
    await transport.close().catch(() => undefined);
  }
}

async function compileSchema(name: string, schema: JsonSchema): Promise<string> {
  return compile(schema, name, {
    bannerComment: "",
    format: false,
    unknownAny: false,
    strictIndexSignatures: false,
  });
}

function indentBlock(input: string): string {
  return input
    .split("\n")
    .map((line) => (line.length > 0 ? `  ${line}` : line))
    .join("\n");
}

async function generate(options: Options): Promise<void> {
  const tools =
    options.source === "live"
      ? await loadToolsFromLive(options.mcpUrl)
      : await loadToolsFromSnapshot(options.snapshotFile);

  const sorted = [...tools].sort((a, b) => a.name.localeCompare(b.name));

  const sections: string[] = [];
  const names: string[] = [];
  const mapLines: string[] = [];

  for (const tool of sorted) {
    const pascal = toPascalCase(tool.name);
    const inputType = `${pascal}Input`;
    const outputType = `${pascal}Output`;
    const inputNamespace = `${pascal}InputSchema`;
    const outputNamespace = `${pascal}OutputSchema`;

    const inputSchema = normalizeSchema(tool.inputSchema ?? tool.input_schema);
    const outputSchema = normalizeSchema(tool.outputSchema ?? tool.output_schema);

    names.push(`"${tool.name}"`);
    mapLines.push(`  "${tool.name}": { input: ${inputType}; output: ${outputType} };`);

    try {
      const compiledInput = await compileSchema(inputType, inputSchema);
      sections.push(
        `export namespace ${inputNamespace} {\n${indentBlock(compiledInput)}\n}\nexport type ${inputType} = ${inputNamespace}.${inputType};`
      );
    } catch {
      sections.push(`export type ${inputType} = Record<string, unknown>;`);
    }

    try {
      const compiledOutput = await compileSchema(outputType, outputSchema);
      sections.push(
        `export namespace ${outputNamespace} {\n${indentBlock(compiledOutput)}\n}\nexport type ${outputType} = ${outputNamespace}.${outputType};`
      );
    } catch {
      sections.push(`export type ${outputType} = Record<string, unknown>;`);
    }
  }

  const toolNameType =
    names.length > 0
      ? `export type McpToolName = ${names.join(" | ")};`
      : "export type McpToolName = never;";

  const content = [
    "/* eslint-disable */",
    "// Auto-generated by scripts/generate-mcp-types.ts. Do not edit manually.",
    "",
    ...sections,
    "",
    toolNameType,
    "",
    "export interface McpToolMap {",
    ...mapLines,
    "}",
    "",
    "export type McpToolInput<TName extends McpToolName> = McpToolMap[TName][\"input\"];",
    "export type McpToolOutput<TName extends McpToolName> = McpToolMap[TName][\"output\"];",
    "",
  ].join("\n");

  await mkdir(path.dirname(options.outFile), { recursive: true });
  await writeFile(options.outFile, content, "utf8");
}

async function main(): Promise<void> {
  const options = parseArgs();
  await generate(options);
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`Failed to generate MCP types: ${message}\n`);
  process.exitCode = 1;
});
