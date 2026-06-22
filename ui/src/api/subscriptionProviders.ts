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

/**
 * Minimal shape the grouping helper needs from a connected provider. Kept
 * structural (not the generated `ConnectedProvider`) so the helper stays
 * trivially unit-testable without the MCP types.
 */
export interface GroupableProvider {
  id: string;
  name: string;
  /** Provider env vars; the FIRST is the primary credential / account key. */
  env_vars: string[];
}

/**
 * One "account" card: every connected provider id that shares a single
 * underlying credential (the same primary env var). Many regional / plan-tier
 * provider ids (e.g. all four Xiaomi token-plan endpoints) are the same paid
 * account behind one API key, so they collapse into one removable card.
 */
export interface SubscriptionGroup {
  /** Stable key for the group — the shared primary env var (or provider id). */
  key: string;
  /** Human display name for the account (see `GROUP_DISPLAY_NAMES`). */
  name: string;
  /** Every connected provider id in the group (Remove disconnects all of them). */
  providerIds: string[];
}

/**
 * Display-name overrides for known shared-credential accounts, keyed by the
 * primary env var. Without an override the group falls back to the shortest /
 * cleanest member provider name, which is fine for one-off subscriptions but
 * reads oddly for multi-endpoint accounts (e.g. "Z.AI" vs "Zhipu AI Coding
 * Plan" for the same `ZHIPU_API_KEY`).
 */
const GROUP_DISPLAY_NAMES: Record<string, string> = {
  XIAOMI_API_KEY: "Xiaomi",
  ZHIPU_API_KEY: "Z.AI / Zhipu",
  OPENCODE_API_KEY: "OpenCode",
};

/** The grouping key for a provider: its primary env var, or its id if it has none. */
function accountKey(provider: GroupableProvider): string {
  return provider.env_vars[0] ?? provider.id;
}

/**
 * Collapse connected providers into one card per underlying account.
 *
 * Providers sharing a primary env var (`env_vars[0]`) are the same paid
 * subscription connected via one stored credential, so they render as a single
 * card whose Remove disconnects every member provider id. Group order follows
 * first-appearance of the input (callers pre-sort by name); member ids keep
 * input order so the canonical (shortest-id) base provider tends to lead.
 */
export function groupSubscriptionsByAccount(
  providers: GroupableProvider[],
): SubscriptionGroup[] {
  const groups = new Map<string, SubscriptionGroup>();
  for (const provider of providers) {
    const key = accountKey(provider);
    const existing = groups.get(key);
    if (existing) {
      existing.providerIds.push(provider.id);
      continue;
    }
    groups.set(key, {
      key,
      name: GROUP_DISPLAY_NAMES[key] ?? provider.name,
      providerIds: [provider.id],
    });
  }
  return Array.from(groups.values());
}
