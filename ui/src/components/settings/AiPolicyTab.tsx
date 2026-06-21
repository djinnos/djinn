import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { InlineError } from "@/components/InlineError";
import {
  type Jurisdiction,
  type LockLevel,
  type OrgPolicy,
  type SubscriptionPolicyItem,
  chinaHostedSubscriptionIds,
  fetchOrgPolicy,
  jurisdictionLabel,
  saveOrgPolicy,
} from "@/api/orgPolicy";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";

const orgPolicyKey = ["org-policy"] as const;

/** Display order for jurisdiction groups in the allow/block table. */
const JURISDICTION_ORDER: Jurisdiction[] = ["us", "eu", "cn", "other"];

const JURISDICTION_BADGE: Record<Jurisdiction, string> = {
  us: "bg-blue-500/10 text-blue-600 dark:text-blue-400",
  eu: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
  cn: "bg-red-500/10 text-red-600 dark:text-red-400",
  other: "bg-muted text-muted-foreground",
};

/**
 * Admin-only "AI Policy" surface (slice 5 of p8py). Lets an admin block/allow
 * subscription providers — framed by data residency — and set the org-default
 * lane lock level. Blocked subscriptions are filtered out of every member's
 * provider/model list server-side; this screen is the only place they appear.
 */
export function AiPolicyTab() {
  const queryClient = useQueryClient();
  const {
    data: policy,
    isLoading,
    error,
    refetch,
  } = useQuery({
    queryKey: orgPolicyKey,
    queryFn: fetchOrgPolicy,
  });

  const mutation = useMutation({
    mutationFn: saveOrgPolicy,
    onSuccess: (next) => {
      queryClient.setQueryData(orgPolicyKey, next);
      showToast.success("AI policy updated");
    },
    onError: (e: unknown) => {
      showToast.error("Failed to update AI policy", {
        description: e instanceof Error ? e.message : String(e),
      });
    },
  });

  if (isLoading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading AI policy...</div>;
  }
  if (error || !policy) {
    return (
      <InlineError
        message={error instanceof Error ? error.message : "Failed to load AI policy"}
        onRetry={() => void refetch()}
      />
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <SubscriptionPolicySection policy={policy} saving={mutation.isPending} onSave={mutation.mutate} />
      <LaneLockSection policy={policy} saving={mutation.isPending} onSave={mutation.mutate} />
    </div>
  );
}

interface SectionProps {
  policy: OrgPolicy;
  saving: boolean;
  onSave: (patch: { blockedSubscriptions?: string[]; lockLevel?: LockLevel }) => void;
}

function SubscriptionPolicySection({ policy, saving, onSave }: SectionProps) {
  // Local optimistic copy of the blocked set so toggles feel instant; the
  // server response replaces it on save.
  const [blocked, setBlocked] = useState<Set<string>>(
    () => new Set(policy.blockedSubscriptions.map((s) => s.toLowerCase())),
  );

  const grouped = useMemo(() => {
    const by: Record<Jurisdiction, SubscriptionPolicyItem[]> = { us: [], eu: [], cn: [], other: [] };
    for (const sub of policy.subscriptions) by[sub.jurisdiction].push(sub);
    return by;
  }, [policy.subscriptions]);

  const isBlocked = (id: string) => blocked.has(id.toLowerCase());

  function persist(nextBlocked: Set<string>) {
    setBlocked(nextBlocked);
    onSave({ blockedSubscriptions: Array.from(nextBlocked) });
  }

  function toggle(id: string) {
    const next = new Set(blocked);
    if (next.has(id.toLowerCase())) next.delete(id.toLowerCase());
    else next.add(id.toLowerCase());
    persist(next);
  }

  function blockAllChina() {
    const next = new Set(blocked);
    for (const id of chinaHostedSubscriptionIds(policy.subscriptions)) next.add(id.toLowerCase());
    persist(next);
  }

  const hasChina = policy.subscriptions.some((s) => s.jurisdiction === "cn");

  return (
    <section className="flex flex-col gap-3">
      <div>
        <h2 className="text-xl font-bold text-foreground">Subscription access</h2>
        <p className="text-sm text-muted-foreground">
          Block or allow personal-subscription providers org-wide, framed by data residency.
          Blocked subscriptions are hidden from every member&apos;s provider and model lists and
          can&apos;t be selected. API keys provisioned by your operator are never affected.
        </p>
      </div>

      {hasChina && (
        <div>
          <Button
            variant="outline"
            size="sm"
            disabled={saving}
            onClick={blockAllChina}
            data-testid="block-all-china"
          >
            Block all China-hosted subscriptions
          </Button>
        </div>
      )}

      {policy.subscriptions.length === 0 ? (
        <p className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-4 text-sm text-muted-foreground">
          No subscription providers are available in the catalog.
        </p>
      ) : (
        <div className="flex flex-col gap-4">
          {JURISDICTION_ORDER.filter((j) => grouped[j].length > 0).map((j) => (
            <div key={j} className="flex flex-col gap-2">
              <div className="flex items-center gap-2">
                <Badge variant="outline" className={cn("border-transparent", JURISDICTION_BADGE[j])}>
                  {jurisdictionLabel(j)}
                </Badge>
              </div>
              <ul className="flex flex-col divide-y divide-border rounded-lg border border-border bg-card">
                {grouped[j].map((sub) => (
                  <li key={sub.id} className="flex items-center justify-between gap-3 px-4 py-3">
                    <div className="min-w-0">
                      <div className="truncate text-sm font-medium text-foreground">{sub.name}</div>
                      <div className="truncate font-mono text-xs text-muted-foreground">{sub.id}</div>
                    </div>
                    <div className="flex items-center gap-2">
                      <span className="text-xs text-muted-foreground">
                        {isBlocked(sub.id) ? "Blocked" : "Allowed"}
                      </span>
                      <Switch
                        aria-label={`${isBlocked(sub.id) ? "Allow" : "Block"} ${sub.name}`}
                        checked={!isBlocked(sub.id)}
                        disabled={saving}
                        onCheckedChange={() => toggle(sub.id)}
                      />
                    </div>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function LaneLockSection({ policy, saving, onSave }: SectionProps) {
  const locked = policy.lockLevel === "locked";
  return (
    <section className="flex flex-col gap-3 border-t border-border pt-6">
      <div>
        <h2 className="text-xl font-bold text-foreground">Model role lock</h2>
        <p className="text-sm text-muted-foreground">
          When locked, the org-default model role assignment is authoritative — members can&apos;t
          override their plan / implement / review lanes. When flexible, the default only seeds new
          members and they may change it.
        </p>
      </div>
      <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-card px-4 py-3">
        <div>
          <div className="text-sm font-medium text-foreground">Lock member lane assignment</div>
          <div className="text-xs text-muted-foreground">
            {locked ? "Locked — members cannot edit lanes" : "Flexible — members may override"}
          </div>
        </div>
        <Switch
          aria-label="Lock member lane assignment"
          checked={locked}
          disabled={saving}
          onCheckedChange={(next) => onSave({ lockLevel: next ? "locked" : "flexible" })}
        />
      </div>
    </section>
  );
}
