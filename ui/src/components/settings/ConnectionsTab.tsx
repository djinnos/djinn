import { useCallback, useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  CheckmarkCircle04Icon,
  LockIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { ConfirmButton } from "@/components/ConfirmButton";
import { CodexSignInRow } from "@/components/CodexSignInCard";
import { InlineError } from "@/components/InlineError";
import { ApiKeyConnectForm } from "@/components/userConfig/ProviderSection";
import { userConfigKeys } from "@/components/userConfig/userConfigKeys";
import {
  type ConnectedProvider,
  SELF_TARGET,
  fetchUserCatalog,
  fetchUserConnectedProviders,
} from "@/api/userConfig";
import { removeProviderFull } from "@/api/server";
import {
  type SubscriptionGroup,
  groupSubscriptionsByAccount,
  isSubscriptionProvider,
} from "@/api/subscriptionProviders";
import { showToast } from "@/lib/toast";

/**
 * Connections tab — provider auth for the signed-in user. Every connection is a
 * compact row in one of two buckets:
 *
 *  - **Your subscriptions** — personal subscriptions (ChatGPT/Codex OAuth + the
 *    BYO-key coding plans). Per-user and removable; removing deletes only the
 *    caller's own credential server-side. Connected coding-plan providers that
 *    share one underlying credential (same primary env var — e.g. all four
 *    Xiaomi token-plan endpoints, or Z.AI + Zhipu coding plans) collapse into a
 *    single account card whose Remove disconnects the whole group.
 *  - **Provided by your org** — operator/admin-provisioned API keys (Anthropic,
 *    OpenAI API, Google, …). Shown as managed and locked (not removable from
 *    here — they're set via Helm/operator config).
 */
export function ConnectionsTab() {
  const queryClient = useQueryClient();

  const connected = useQuery({
    queryKey: userConfigKeys.connectedProviders(SELF_TARGET),
    queryFn: () => fetchUserConnectedProviders(SELF_TARGET),
  });
  const catalog = useQuery({
    queryKey: userConfigKeys.catalog(SELF_TARGET),
    queryFn: () => fetchUserCatalog(SELF_TARGET),
  });

  const refresh = useCallback(() => {
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedProviders(SELF_TARGET),
    });
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedModels(SELF_TARGET),
    });
  }, [queryClient]);

  // ChatGPT/Codex connects under the `openai` provider via builtin merge — an
  // `oauth` connection method on `openai` means Codex is signed in. (A plain
  // `openai` API key, by contrast, is an org-provided key.)
  const openaiProvider = (connected.data ?? []).find((p) => p.id === "openai");
  const codexConnected = openaiProvider?.connection_methods.includes("oauth") ?? false;
  const codexRevokedReason = (
    openaiProvider as { revoked_reason?: string } | undefined
  )?.revoked_reason;

  // Split connected providers into the two buckets, then collapse subscriptions
  // by shared credential. The Codex sub is rendered by its dedicated OAuth row,
  // so an `openai`-via-oauth row is excluded here to avoid a duplicate; a plain
  // `openai` API key still lands in the org bucket.
  const { subscriptionGroups, orgProvided } = useMemo(() => {
    const subs: ConnectedProvider[] = [];
    const org: ConnectedProvider[] = [];
    for (const provider of connected.data ?? []) {
      const isCodexRow =
        provider.id === "openai" && provider.connection_methods.includes("oauth");
      if (isCodexRow) continue; // owned by the Codex row below
      if (isSubscriptionProvider(provider.id)) subs.push(provider);
      else org.push(provider);
    }
    const byName = (a: ConnectedProvider, b: ConnectedProvider) =>
      a.name.localeCompare(b.name);
    subs.sort(byName);
    return {
      subscriptionGroups: groupSubscriptionsByAccount(subs),
      orgProvided: org.sort(byName),
    };
  }, [connected.data]);

  if (connected.isError) {
    return (
      <InlineError
        message={
          connected.error instanceof Error
            ? connected.error.message
            : "Failed to load connected providers"
        }
        onRetry={() => void connected.refetch()}
      />
    );
  }

  const loading = connected.isLoading;

  return (
    <div className="flex flex-col gap-8">
      {/* ── Your subscriptions ─────────────────────────────────────────── */}
      <section className="flex flex-col gap-3">
        <div>
          <h2 className="text-xl font-bold text-foreground">Your subscriptions</h2>
          <p className="text-sm text-muted-foreground">
            Personal subscriptions connected to your account — sign in with ChatGPT,
            or paste a coding-plan key (Kimi, MiniMax, Z.AI, Xiaomi, OpenCode…). These
            are yours and you can remove them at any time.
          </p>
        </div>

        {loading ? (
          <CardSkeleton />
        ) : (
          <ul className="flex flex-col gap-2">
            <CodexSignInRow
              alreadyConnected={codexConnected}
              revokedReason={codexRevokedReason}
              onConnected={refresh}
            />
            {subscriptionGroups.map((group) => (
              <SubscriptionGroupCard key={group.key} group={group} onRemoved={refresh} />
            ))}
          </ul>
        )}

        <ApiKeyConnectForm
          targetId={SELF_TARGET}
          catalog={catalog.data ?? []}
          connectedIds={new Set((connected.data ?? []).map((p) => p.id))}
          catalogLoading={catalog.isLoading}
          onConnected={refresh}
        />
      </section>

      {/* ── Provided by your org ───────────────────────────────────────── */}
      <section className="flex flex-col gap-3">
        <div>
          <h2 className="text-xl font-bold text-foreground">Provided by your org</h2>
          <p className="text-sm text-muted-foreground">
            API keys your operator provisions for everyone (Anthropic, OpenAI API,
            Google, Azure, AWS, Vertex AI…). They&apos;re managed centrally — ask your
            operator to change or unset a key.
          </p>
        </div>

        {loading ? (
          <CardSkeleton />
        ) : orgProvided.length === 0 ? (
          <p className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-4 text-sm text-muted-foreground">
            No org-provided API keys yet. Your operator can set Helm-managed keys.
          </p>
        ) : (
          <ul className="flex flex-col gap-2">
            {orgProvided.map((provider) => (
              <OrgProvidedCard key={provider.id} provider={provider} />
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

function CardSkeleton() {
  return (
    <div className="rounded-lg border border-border bg-card px-4 py-3 text-sm text-muted-foreground">
      Loading…
    </div>
  );
}

/**
 * A connected personal-subscription account — a first-class row, removable by
 * the owner. One row stands for every provider id that shares the account's
 * credential, so Remove disconnects all of them.
 */
function SubscriptionGroupCard({
  group,
  onRemoved,
}: {
  group: SubscriptionGroup;
  onRemoved: () => void;
}) {
  const remove = useMutation({
    // Disconnect every provider id in the group. They share one credential, but
    // removing each by id is idempotent server-side and clears any per-id state.
    mutationFn: () => Promise.all(group.providerIds.map((id) => removeProviderFull(id))),
    onSuccess: () => {
      showToast.success("Subscription removed", {
        description: `Disconnected ${group.name}.`,
      });
      onRemoved();
    },
    onError: (error) => {
      showToast.error("Could not remove subscription", {
        description: error instanceof Error ? error.message : "Unknown error",
      });
    },
  });

  const planCount = group.providerIds.length;
  const subtitle =
    planCount > 1
      ? `Connected · personal subscription · ${planCount} plans`
      : "Connected · personal subscription";

  return (
    <li className="flex items-center justify-between gap-3 rounded-lg border border-border bg-card px-4 py-3">
      <div className="flex min-w-0 items-center gap-2.5">
        <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="shrink-0 text-green-500" />
        <div className="min-w-0">
          <div className="truncate text-sm font-medium text-foreground">{group.name}</div>
          <div className="truncate text-xs text-muted-foreground">{subtitle}</div>
        </div>
      </div>
      <ConfirmButton
        title="Remove subscription"
        description={
          planCount > 1
            ? `Disconnect "${group.name}" and delete the stored key for your account? This removes all ${planCount} connected plans on this account.`
            : `Disconnect "${group.name}" and delete the stored key/token for your account?`
        }
        confirmLabel="Remove"
        onConfirm={() => remove.mutate()}
        size="sm"
        variant="outline"
        disabled={remove.isPending}
      >
        {remove.isPending ? "Removing…" : "Remove"}
      </ConfirmButton>
    </li>
  );
}

/** A connected org-provided API key — managed/locked, not removable here. */
function OrgProvidedCard({ provider }: { provider: ConnectedProvider }) {
  return (
    <li
      className="flex items-center justify-between gap-3 rounded-lg border border-border bg-card px-4 py-3"
      title="Provisioned via deployment (Helm). Ask your operator to unset the key."
    >
      <div className="flex min-w-0 items-center gap-2.5">
        <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="shrink-0 text-green-500" />
        <div className="min-w-0">
          <div className="truncate text-sm font-medium text-foreground">{provider.name}</div>
          <div className="truncate text-xs text-muted-foreground">Managed by your org</div>
        </div>
      </div>
      <span className="flex shrink-0 items-center gap-1.5 rounded-md border border-border px-2.5 py-1 text-xs text-muted-foreground">
        <HugeiconsIcon icon={LockIcon} size={13} aria-label="Provisioned via deployment" />
        Managed
      </span>
    </li>
  );
}
