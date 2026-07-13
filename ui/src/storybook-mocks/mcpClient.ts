/**
 * Storybook mock for `@/api/mcpClient`.
 *
 * `storybook dev` runs on the Vite builder, where vitest's `vi.mock(...)` does
 * NOT work — its bundled chai/expect setup throws
 * "Cannot read properties of undefined (reading 'customEqualityTesters')" in
 * the browser preview runtime, and Storybook 9.1's experimental `sb.mock()` is
 * a no-op stub under Vite (module substitution is only wired for the webpack
 * builder). So instead, `.storybook/main.ts` aliases `@/api/mcpClient` to this
 * module at bundle time.
 *
 * The alias is GLOBAL — every story gets this `callMcpTool`. By default it
 * behaves as if the backend is absent (the returned promise rejects), so
 * stories that never opt in (e.g. Pages/Settings, which seeds TanStack Query
 * directly) settle into exactly the error/empty states they already reach
 * against the real, unreachable Storybook server.
 *
 * A story opts in by calling `setMcpToolResponder` in a decorator or
 * `beforeEach` to install a per-tool handler. Module-level state resets on full
 * page reload, which Storybook performs when navigating between stories.
 */
import type { McpToolInput, McpToolName, McpToolOutput } from "@/api/generated/mcp-tools.gen";

export type McpToolResponder = (
  name: string,
  args: Record<string, unknown> | undefined,
) => unknown | Promise<unknown>;

const backendAbsent: McpToolResponder = (name) =>
  Promise.reject(new Error(`MCP backend unavailable in Storybook (tool: ${name})`));

let responder: McpToolResponder = backendAbsent;

/**
 * Install a per-tool responder for the current story. Return a value (or a
 * promise) to resolve the call, or throw / reject to drive an error path.
 */
export function setMcpToolResponder(next: McpToolResponder): void {
  responder = next;
}

/** Restore the default "backend absent" behavior. */
export function resetMcpToolResponder(): void {
  responder = backendAbsent;
}

export async function callMcpTool<TName extends McpToolName>(
  name: TName,
  args?: McpToolInput<TName>,
): Promise<McpToolOutput<TName>> {
  // A third `options` arg (timeout) is accepted by the real client and simply
  // ignored here — extra positional args are harmless at the call site.
  return (await responder(name, args as Record<string, unknown> | undefined)) as McpToolOutput<TName>;
}

/** No-op in Storybook — there is no live client to reset. */
export async function resetMcpClient(): Promise<void> {}
