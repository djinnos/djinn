/**
 * Client-side mirror of the server's subscription/API-key provider
 * classification (`djinn_provider::catalog::builtin::is_subscription_provider`).
 *
 * Personal subscriptions (ChatGPT/Codex, Kimi, MiniMax, Z.AI, Zhipu, Xiaomi,
 * OpenCode, …) are connected per-user and removable by their owner; everything
 * else (Anthropic, OpenAI API, Google, Azure, AWS, Vertex, Fireworks, generic
 * OpenAI-compatible keys) is a fungible API key that — in the hosted product —
 * the operator provisions for the org. The Connections tab uses this split to
 * sort connected providers into the "Your subscriptions" vs "Provided by your
 * org" buckets, mirroring the server's per-user credential ownership rules.
 *
 * Kept in lockstep with the Rust source; the set is small and stable.
 */

/** Exact subscription provider ids (hand-registered builtins + models.dev-native). */
const SUBSCRIPTION_IDS = new Set([
  "chatgpt_codex",
  "minimax-coding-plan",
  "kimi-for-coding",
  "opencode",
  "opencode-go",
  "zai",
  "zai-coding-plan",
  "zhipuai",
  "zhipuai-coding-plan",
]);

/** Consumer-subscription vendor prefixes (regional/plan-tier variants). */
const SUBSCRIPTION_PREFIXES = [
  "xiaomi",
  "moonshotai",
  "alibaba",
  "tencent",
  "stepfun",
  "kuae-cloud",
  "umans-ai",
];

/** Coding/token-plan id suffixes used across vendors' consumer plans. */
const SUBSCRIPTION_SUFFIXES = ["-coding-plan", "-token-plan", "-for-coding"];

/**
 * True when `providerId` is a personal subscription (per-user, removable).
 *
 * NOTE on `openai`: the ChatGPT/Codex OAuth subscription is merged into the
 * `openai` provider id server-side, so a connected `openai` row can be EITHER a
 * Codex subscription (oauth) or a plain BYO API key. Callers that need to tell
 * them apart should inspect the connection method (`oauth` ⇒ Codex sub); this
 * helper alone reports `openai` as NOT a subscription (its API-key identity).
 */
export function isSubscriptionProvider(providerId: string): boolean {
  const id = providerId.toLowerCase();
  if (SUBSCRIPTION_IDS.has(id)) return true;
  if (SUBSCRIPTION_SUFFIXES.some((suffix) => id.includes(suffix))) return true;
  if (SUBSCRIPTION_PREFIXES.some((prefix) => id.startsWith(prefix))) return true;
  return false;
}
