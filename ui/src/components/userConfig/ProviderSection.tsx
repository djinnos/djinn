import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertCircleIcon,
  CheckmarkCircle04Icon,
  Copy01Icon,
  LinkForwardIcon,
  Loading02Icon,
  SparklesIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { InlineError } from "@/components/InlineError";
import {
  type CatalogProvider,
  type ConnectedProvider,
  fetchUserCatalog,
  fetchUserConnectedProviders,
  setUserCredential,
  startUserOAuth,
  type UserOAuthPending,
} from "@/api/userConfig";
import { getServerBaseUrl } from "@/api/serverUrl";
import { showToast } from "@/lib/toast";

import { userConfigKeys } from "./userConfigKeys";

export function ProviderSection({ targetId }: { targetId: string }) {
  const queryClient = useQueryClient();

  const connected = useQuery({
    queryKey: userConfigKeys.connectedProviders(targetId),
    queryFn: () => fetchUserConnectedProviders(targetId),
  });
  const catalog = useQuery({
    queryKey: userConfigKeys.catalog(targetId),
    queryFn: () => fetchUserCatalog(targetId),
  });

  const refreshConnected = useCallback(() => {
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedProviders(targetId),
    });
    // Connecting a provider unlocks new models too.
    void queryClient.invalidateQueries({
      queryKey: userConfigKeys.connectedModels(targetId),
    });
  }, [queryClient, targetId]);

  const connectedIds = useMemo(
    () => new Set((connected.data ?? []).map((p) => p.id)),
    [connected.data],
  );

  // Codex connects under the `openai` provider via builtin merge; an `oauth`
  // connection method on `openai` means Codex is signed in for this user.
  const codexConnected = (connected.data ?? []).some(
    (p) => p.id === "openai" && p.connection_methods.includes("oauth"),
  );

  return (
    <section className="flex flex-col gap-3">
      <div>
        <h3 className="text-base font-semibold text-foreground">Providers</h3>
        <p className="text-sm text-muted-foreground">
          Connect a model provider for this user. Use the device-code
          sign-in for ChatGPT / Codex, or paste an API key for any other provider.
        </p>
      </div>

      {connected.isError ? (
        <InlineError
          message={
            connected.error instanceof Error
              ? connected.error.message
              : "Failed to load connected providers"
          }
          onRetry={() => void connected.refetch()}
        />
      ) : (
        <div className="rounded-lg border border-border bg-card px-4 py-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="shrink-0 text-sm font-medium text-muted-foreground">
              Connected:
            </span>
            {connected.isLoading ? (
              <span className="text-sm text-muted-foreground">Loading…</span>
            ) : (connected.data?.length ?? 0) === 0 ? (
              <span className="text-sm text-muted-foreground">
                None yet — connect a provider below.
              </span>
            ) : (
              (connected.data ?? []).map((provider: ConnectedProvider) => (
                <span
                  key={provider.id}
                  className="flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1 text-sm"
                >
                  <HugeiconsIcon
                    icon={CheckmarkCircle04Icon}
                    size={13}
                    className="text-green-500"
                  />
                  {provider.name}
                </span>
              ))
            )}
          </div>
        </div>
      )}

      <CodexConnectCard
        targetId={targetId}
        alreadyConnected={codexConnected}
        onConnected={refreshConnected}
      />

      <ApiKeyConnectForm
        targetId={targetId}
        catalog={catalog.data ?? []}
        connectedIds={connectedIds}
        catalogLoading={catalog.isLoading}
        onConnected={refreshConnected}
      />
    </section>
  );
}

export function ApiKeyConnectForm({
  targetId,
  catalog,
  connectedIds,
  catalogLoading,
  onConnected,
}: {
  targetId: string;
  catalog: CatalogProvider[];
  connectedIds: Set<string>;
  catalogLoading: boolean;
  onConnected: () => void;
}) {
  const [providerId, setProviderId] = useState("");
  const [apiKey, setApiKey] = useState("");

  // Only providers that take an API key (have at least one env var) belong in
  // the manual-key form; pure-OAuth providers (e.g. Codex) connect above.
  const keyProviders = useMemo(
    () => catalog.filter((p) => p.env_vars.length > 0),
    [catalog],
  );
  // base-ui's Combobox takes a flat `items` list + an id→label resolver so the
  // trigger renders the chosen provider's name (not its raw id).
  const keyProviderIds = useMemo(() => keyProviders.map((p) => p.id), [keyProviders]);
  const providerLabel = useMemo(
    () => new Map(keyProviders.map((p) => [p.id, p.name])),
    [keyProviders],
  );

  const selected = keyProviders.find((p) => p.id === providerId);
  const keyName = selected?.env_vars[0] ?? "";

  const mutation = useMutation({
    mutationFn: () =>
      setUserCredential({
        targetUserId: targetId,
        providerId,
        keyName,
        apiKey: apiKey.trim(),
      }),
    onSuccess: () => {
      showToast.success("Provider connected", {
        description: `${selected?.name ?? providerId} key stored for this user.`,
      });
      setApiKey("");
      setProviderId("");
      onConnected();
    },
    onError: (error) => {
      showToast.error("Could not connect provider", {
        description: error instanceof Error ? error.message : "Unknown error",
      });
    },
  });

  const canSubmit = Boolean(providerId && keyName && apiKey.trim()) && !mutation.isPending;

  return (
    <form
      className="flex flex-col gap-3 rounded-lg border border-border bg-card p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (canSubmit) mutation.mutate();
      }}
    >
      <div className="text-sm font-medium text-foreground">Connect with an API key</div>

      <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
        <div className="flex flex-1 flex-col gap-1.5">
          <Label htmlFor="user-config-provider">Provider</Label>
          <Combobox
            items={keyProviderIds}
            value={providerId || null}
            onValueChange={(v) => setProviderId(typeof v === "string" ? v : "")}
            itemToStringLabel={(id) => providerLabel.get(id) ?? id}
            disabled={catalogLoading || mutation.isPending}
          >
            <ComboboxInput
              id="user-config-provider"
              showClear={Boolean(providerId)}
              placeholder={catalogLoading ? "Loading providers…" : "Select a provider…"}
              className="w-full"
            />
            <ComboboxContent>
              <ComboboxList>
                <ComboboxEmpty>No providers found</ComboboxEmpty>
                {keyProviders.map((p) => (
                  <ComboboxItem key={p.id} value={p.id}>
                    <span className="truncate">{p.name}</span>
                    {connectedIds.has(p.id) ? (
                      <span className="ml-1.5 text-xs text-muted-foreground">connected</span>
                    ) : null}
                  </ComboboxItem>
                ))}
              </ComboboxList>
            </ComboboxContent>
          </Combobox>
        </div>

        <div className="flex flex-[2] flex-col gap-1.5">
          <Label htmlFor="user-config-api-key">
            API key{keyName ? ` (${keyName})` : ""}
          </Label>
          <Input
            id="user-config-api-key"
            type="password"
            autoComplete="off"
            placeholder="sk-…"
            value={apiKey}
            disabled={!providerId || mutation.isPending}
            onChange={(e) => setApiKey(e.target.value)}
          />
        </div>

        <Button type="submit" disabled={!canSubmit} className="shrink-0">
          {mutation.isPending ? (
            <>
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
              Saving…
            </>
          ) : (
            "Connect"
          )}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        Stored encrypted and owned by this user.
      </p>
    </form>
  );
}

type CodexPhase =
  | { kind: "idle" }
  | { kind: "pending"; pending: UserOAuthPending }
  | { kind: "error"; message: string }
  | { kind: "just_connected" };

export function CodexConnectCard({
  targetId,
  alreadyConnected,
  onConnected,
}: {
  targetId: string;
  alreadyConnected: boolean;
  onConnected: () => void;
}) {
  const [phase, setPhase] = useState<CodexPhase>({ kind: "idle" });
  // Mark completion in a ref the SSE/poll watchers read so they don't race a
  // unmount; `phase` drives the render.
  const settledRef = useRef(false);

  const showConnected =
    phase.kind === "just_connected" || (phase.kind === "idle" && alreadyConnected);

  const handleConnect = useCallback(async () => {
    settledRef.current = false;
    setPhase({ kind: "idle" });
    const result = await startUserOAuth(targetId, "openai");
    if (result.kind === "connected") {
      settledRef.current = true;
      setPhase({ kind: "just_connected" });
      onConnected();
      return;
    }
    if (result.kind === "pending") {
      setPhase({ kind: "pending", pending: result.pending });
      return;
    }
    setPhase({ kind: "error", message: result.message });
  }, [targetId, onConnected]);

  // While a device-code flow is pending, await the server's `credential.updated`
  // SSE (the same signal the self-connect flow uses) and poll
  // `provider_connected` as a fallback. On either signal we flip to connected
  // and let the parent refresh. State stays entirely local to this card.
  useEffect(() => {
    if (phase.kind !== "pending") return;

    let active = true;
    const markConnected = () => {
      if (!active || settledRef.current) return;
      settledRef.current = true;
      setPhase({ kind: "just_connected" });
      onConnected();
    };

    const es = new EventSource(`${getServerBaseUrl()}/events`);
    const onCredentialEvent = () => {
      // The SSE carries no target id, so re-check this user's connected
      // providers before declaring success.
      void fetchUserConnectedProviders(targetId)
        .then((providers) => {
          if (providers.some((p) => p.id === "openai" && p.connection_methods.includes("oauth"))) {
            markConnected();
          }
        })
        .catch(() => undefined);
    };
    es.addEventListener("credential.updated", onCredentialEvent);
    es.addEventListener("credential.created", onCredentialEvent);

    // Fallback poll in case the SSE is missed (component opened mid-stream).
    const interval = setInterval(() => {
      void fetchUserConnectedProviders(targetId)
        .then((providers) => {
          if (providers.some((p) => p.id === "openai" && p.connection_methods.includes("oauth"))) {
            markConnected();
          }
        })
        .catch(() => undefined);
    }, 3000);

    // Give up when the device code expires.
    const expiry = setTimeout(
      () => {
        if (active && !settledRef.current) {
          setPhase({ kind: "error", message: "Sign-in timed out. Try again." });
        }
      },
      Math.max(phase.pending.expiresInSecs, 60) * 1000,
    );

    return () => {
      active = false;
      es.close();
      clearInterval(interval);
      clearTimeout(expiry);
    };
  }, [phase, targetId, onConnected]);

  const handleCopy = async (code: string) => {
    try {
      await navigator.clipboard.writeText(code);
      showToast.success("Code copied");
    } catch {
      showToast.error("Could not copy", { description: "Copy the code manually." });
    }
  };

  return (
    <div className="relative flex flex-col gap-3 overflow-hidden rounded-2xl border border-primary/40 bg-gradient-to-br from-primary/[0.06] to-transparent p-5">
      <div className="pointer-events-none absolute -right-8 -top-8 h-24 w-24 rounded-full bg-primary/20 blur-3xl" />
      <div className="flex items-center gap-3">
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-primary/15">
          <HugeiconsIcon icon={SparklesIcon} size={18} className="text-primary" />
        </div>
        <div>
          <h4 className="text-sm font-semibold text-foreground">ChatGPT / Codex</h4>
          <p className="text-xs text-muted-foreground">Sign in with a device code</p>
        </div>
      </div>

      {phase.kind === "idle" && !showConnected && (
        <>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Connect a ChatGPT Plus, Pro, or Team account for this user.
            No API key needed.
          </p>
          <Button className="w-full" onClick={() => void handleConnect()}>
            Continue with ChatGPT
          </Button>
        </>
      )}

      {phase.kind === "pending" && (
        <>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Open the sign-in page and enter this code:
          </p>
          <div className="flex items-center gap-2">
            <code className="flex-1 rounded-lg border border-border bg-card px-4 py-3 text-center text-2xl font-mono font-semibold tracking-widest text-foreground">
              {phase.pending.userCode}
            </code>
            <Button
              type="button"
              variant="outline"
              size="lg"
              aria-label="Copy code"
              onClick={() => void handleCopy(phase.pending.userCode)}
            >
              <HugeiconsIcon icon={Copy01Icon} size={18} />
            </Button>
          </div>
          <p className="flex items-center gap-2 text-xs text-muted-foreground">
            <HugeiconsIcon icon={Loading02Icon} size={14} className="shrink-0 animate-spin" />
            Waiting for sign-in to complete
            {phase.pending.expiresInSecs
              ? ` (expires in ${Math.floor(phase.pending.expiresInSecs / 60)} min)`
              : ""}
            …
          </p>
          <a
            href={phase.pending.verificationUriComplete}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center justify-center gap-2 rounded-md bg-primary px-4 py-2.5 text-sm font-medium text-primary-foreground shadow transition-colors hover:bg-primary/90"
          >
            Open sign-in page
            <HugeiconsIcon icon={LinkForwardIcon} size={16} />
          </a>
        </>
      )}

      {showConnected && (
        <>
          <span className="inline-flex w-fit items-center gap-1.5 rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-500">
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />
            Connected
          </span>
          <Button variant="outline" className="w-full" onClick={() => void handleConnect()}>
            Reconnect
          </Button>
        </>
      )}

      {phase.kind === "error" && (
        <>
          <p className="flex items-start gap-2 text-sm text-destructive">
            <HugeiconsIcon icon={AlertCircleIcon} size={16} className="mt-0.5 shrink-0" />
            <span>{phase.message}</span>
          </p>
          <Button className="w-full" onClick={() => void handleConnect()}>
            Try again
          </Button>
        </>
      )}
    </div>
  );
}

