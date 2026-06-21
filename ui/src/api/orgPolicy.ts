import { callMcpTool } from "@/api/mcpClient";

import { type ModelLanes, parseLanes } from "@/api/userSettings";

/** Data-residency jurisdiction of a subscription provider (djinn-owned map). */
export type Jurisdiction = "us" | "eu" | "cn" | "other";

/** Lane lock level: members may override (`flexible`) or not (`locked`). */
export type LockLevel = "flexible" | "locked";

/** One subscription provider row in the admin allow/block table. */
export interface SubscriptionPolicyItem {
  id: string;
  name: string;
  jurisdiction: Jurisdiction;
  blocked: boolean;
}

/** The org AI policy as the admin surface reads it. */
export interface OrgPolicy {
  subscriptions: SubscriptionPolicyItem[];
  blockedSubscriptions: string[];
  defaultLanes: ModelLanes;
  lockLevel: LockLevel;
}

function normalizeJurisdiction(raw: unknown): Jurisdiction {
  return raw === "us" || raw === "eu" || raw === "cn" ? raw : "other";
}

function normalizeLock(raw: unknown): LockLevel {
  return raw === "locked" ? "locked" : "flexible";
}

/** Read the org AI policy. Admin-only on the server; non-admins get an error. */
export async function fetchOrgPolicy(): Promise<OrgPolicy> {
  const resp = (await callMcpTool("org_policy_get", {})) as {
    ok?: boolean;
    error?: string | null;
    subscriptions?: Array<{
      id?: string;
      name?: string;
      jurisdiction?: string;
      blocked?: boolean;
    }>;
    blocked_subscriptions?: string[];
    default_lanes?: Partial<ModelLanes> | null;
    lock_level?: string;
  };
  if (resp.ok === false) {
    throw new Error(resp.error ?? "failed to load org AI policy");
  }
  return {
    subscriptions: (resp.subscriptions ?? []).map((s) => ({
      id: s.id ?? "",
      name: s.name ?? s.id ?? "",
      jurisdiction: normalizeJurisdiction(s.jurisdiction),
      blocked: s.blocked ?? false,
    })),
    blockedSubscriptions: resp.blocked_subscriptions ?? [],
    defaultLanes: parseLanes(resp.default_lanes),
    lockLevel: normalizeLock(resp.lock_level),
  };
}

/** Patch fields of the org AI policy; omitted fields keep their current value. */
export interface OrgPolicyPatch {
  blockedSubscriptions?: string[];
  defaultLanes?: ModelLanes;
  lockLevel?: LockLevel;
}

export async function saveOrgPolicy(patch: OrgPolicyPatch): Promise<OrgPolicy> {
  const args: Record<string, unknown> = {};
  if (patch.blockedSubscriptions !== undefined) {
    args.blocked_subscriptions = patch.blockedSubscriptions;
  }
  if (patch.defaultLanes !== undefined) {
    args.default_lanes = patch.defaultLanes;
  }
  if (patch.lockLevel !== undefined) {
    args.lock_level = patch.lockLevel;
  }
  const resp = (await callMcpTool("org_policy_set", args)) as {
    ok?: boolean;
    error?: string | null;
  };
  if (resp.ok === false) {
    throw new Error(resp.error ?? "failed to save org AI policy");
  }
  // Re-read for the full table (set returns only the stored fields, not the
  // jurisdiction-annotated subscription universe).
  return fetchOrgPolicy();
}

/** All China-hosted subscription ids in the current table — for the one-click block. */
export function chinaHostedSubscriptionIds(subscriptions: SubscriptionPolicyItem[]): string[] {
  return subscriptions.filter((s) => s.jurisdiction === "cn").map((s) => s.id);
}

const JURISDICTION_LABELS: Record<Jurisdiction, string> = {
  us: "United States",
  eu: "European Union",
  cn: "China",
  other: "Other",
};

export function jurisdictionLabel(j: Jurisdiction): string {
  return JURISDICTION_LABELS[j];
}
