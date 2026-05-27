import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Reorder, useDragControls } from "framer-motion";
import {
  AlertCircleIcon,
  CheckmarkCircle04Icon,
  Copy01Icon,
  Delete02Icon,
  LinkForwardIcon,
  Loading02Icon,
  SparklesIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { InlineError } from "@/components/InlineError";
import {
  ModelSelector,
  ModelSelectorContent,
  ModelSelectorEmpty,
  ModelSelectorGroup,
  ModelSelectorInput,
  ModelSelectorItem,
  ModelSelectorList,
  ModelSelectorLogo,
  ModelSelectorName,
  ModelSelectorSeparator,
  ModelSelectorTrigger,
} from "@/components/ai-elements/model-selector";
import { userDisplayName, type OrgUser } from "@/api/users";
import {
  type AutomationModel,
  type ConnectedProvider,
  fetchAutomationCatalog,
  fetchAutomationConnectedModels,
  fetchAutomationConnectedProviders,
  fetchAutomationModelSelection,
  saveAutomationModelSelection,
  setAutomationCredential,
  startAutomationOAuth,
  type AutomationOAuthPending,
} from "@/api/automationConfig";
import { getServerBaseUrl } from "@/api/serverUrl";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";

/** Known provider display names; falls back to title-casing the id. */
function formatProvider(id: string): string {
  const known: Record<string, string> = {
    openai: "OpenAI",
    anthropic: "Anthropic",
    google: "Google",
    azure: "Azure",
    aws: "AWS",
    "fireworks-ai": "Fireworks",
    mistral: "Mistral",
    cohere: "Cohere",
    groq: "Groq",
    deepseek: "DeepSeek",
    perplexity: "Perplexity",
    chatgpt_codex: "ChatGPT / Codex",
  };
  return (
    known[id.toLowerCase()] ??
    id.replace(/[-_]/g, " ").replace(/\b\w/g, (c) => c.toUpperCase())
  );
}

// react-query keys are scoped by the automation user's id so this component's
// cache never collides with the current-user singletons elsewhere in the app.
const automationKeys = {
  connectedProviders: (id: string) => ["automation", id, "connected-providers"] as const,
  catalog: (id: string) => ["automation", id, "catalog"] as const,
  connectedModels: (id: string) => ["automation", id, "connected-models"] as const,
  modelSelection: (id: string) => ["automation", id, "model-selection"] as const,
};

interface AutomationConfigProps {
  /** The automation service user — its `id` is threaded as `target_user_id`. */
  user: OrgUser;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AutomationConfig({ user, open, onOpenChange }: AutomationConfigProps) {
  const targetId = user.id;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] w-full overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <HugeiconsIcon icon={SparklesIcon} size={18} className="text-primary" />
            Configure automation
          </DialogTitle>
          <DialogDescription>
            Manage credentials and model selection for{" "}
            <span className="font-medium text-foreground">{userDisplayName(user)}</span>
            {" "}— the non-human service account that runs system-initiated work. It
            can't sign in, so you configure it here on its behalf.
          </DialogDescription>
        </DialogHeader>

        {open && (
          <div className="flex flex-col gap-6">
            <ProviderSection targetId={targetId} />
            <Separator />
            <ModelSection targetId={targetId} />
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

// ── Providers ────────────────────────────────────────────────────────────────

function ProviderSection({ targetId }: { targetId: string }) {
  const queryClient = useQueryClient();

  const connected = useQuery({
    queryKey: automationKeys.connectedProviders(targetId),
    queryFn: () => fetchAutomationConnectedProviders(targetId),
  });
  const catalog = useQuery({
    queryKey: automationKeys.catalog(targetId),
    queryFn: () => fetchAutomationCatalog(targetId),
  });

  const refreshConnected = useCallback(() => {
    void queryClient.invalidateQueries({
      queryKey: automationKeys.connectedProviders(targetId),
    });
    // Connecting a provider unlocks new models too.
    void queryClient.invalidateQueries({
      queryKey: automationKeys.connectedModels(targetId),
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
          Connect a model provider for the automation user. Use the device-code
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

function ApiKeyConnectForm({
  targetId,
  catalog,
  connectedIds,
  catalogLoading,
  onConnected,
}: {
  targetId: string;
  catalog: { id: string; name: string; env_vars: string[]; oauth_supported: boolean }[];
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

  const selected = keyProviders.find((p) => p.id === providerId);
  const keyName = selected?.env_vars[0] ?? "";

  const mutation = useMutation({
    mutationFn: () =>
      setAutomationCredential({
        targetUserId: targetId,
        providerId,
        keyName,
        apiKey: apiKey.trim(),
      }),
    onSuccess: () => {
      showToast.success("Provider connected", {
        description: `${selected?.name ?? providerId} key stored for automation.`,
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
          <Label htmlFor="automation-provider">Provider</Label>
          <select
            id="automation-provider"
            value={providerId}
            disabled={catalogLoading || mutation.isPending}
            onChange={(e) => setProviderId(e.target.value)}
            className={cn(
              "h-8 w-full rounded-lg border border-input bg-transparent px-2.5 text-sm outline-none",
              "focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50",
              "disabled:cursor-not-allowed disabled:opacity-50 dark:bg-input/30",
            )}
          >
            <option value="">
              {catalogLoading ? "Loading providers…" : "Select a provider…"}
            </option>
            {keyProviders.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
                {connectedIds.has(p.id) ? " (connected)" : ""}
              </option>
            ))}
          </select>
        </div>

        <div className="flex flex-[2] flex-col gap-1.5">
          <Label htmlFor="automation-api-key">
            API key{keyName ? ` (${keyName})` : ""}
          </Label>
          <Input
            id="automation-api-key"
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
        Stored encrypted and owned by the automation user.
      </p>
    </form>
  );
}

type CodexPhase =
  | { kind: "idle" }
  | { kind: "pending"; pending: AutomationOAuthPending }
  | { kind: "error"; message: string }
  | { kind: "just_connected" };

function CodexConnectCard({
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
    const result = await startAutomationOAuth(targetId, "openai");
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
      void fetchAutomationConnectedProviders(targetId)
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
      void fetchAutomationConnectedProviders(targetId)
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
            Connect a ChatGPT Plus, Pro, or Team account for the automation user.
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

// ── Models ───────────────────────────────────────────────────────────────────

/** Strips the provider prefix from a model id ("openai/gpt-5" → "gpt-5"). */
function stripProviderPrefix(modelId: string): string {
  const slash = modelId.indexOf("/");
  return slash >= 0 ? modelId.slice(slash + 1) : modelId;
}

function ModelSection({ targetId }: { targetId: string }) {
  const queryClient = useQueryClient();

  const connectedModels = useQuery({
    queryKey: automationKeys.connectedModels(targetId),
    queryFn: () => fetchAutomationConnectedModels(targetId),
  });
  const selection = useQuery({
    queryKey: automationKeys.modelSelection(targetId),
    queryFn: () => fetchAutomationModelSelection(targetId),
  });

  // Local working copy of the ordered selection; seeded from the server and
  // kept isolated from the current-user settings store. We sync from the
  // server value during render (the React-recommended "adjust state while
  // rendering" pattern — https://react.dev/reference/react/useState#storing-
  // information-from-previous-renders) instead of an effect: when the admin
  // has unsaved edits (`dirty`) we hold onto their working copy until they
  // save, otherwise we mirror whatever the server last returned.
  const [order, setOrder] = useState<string[]>([]);
  const [dirty, setDirty] = useState(false);
  const [lastServer, setLastServer] = useState<string[] | undefined>(undefined);

  if (selection.data && selection.data !== lastServer) {
    setLastServer(selection.data);
    if (!dirty) {
      setOrder(selection.data);
    }
  }

  const modelsById = useMemo(() => {
    const map = new Map<string, AutomationModel>();
    for (const m of connectedModels.data ?? []) map.set(m.id, m);
    return map;
  }, [connectedModels.data]);

  const availableToAdd = useMemo(
    () => (connectedModels.data ?? []).filter((m) => !order.includes(m.id)),
    [connectedModels.data, order],
  );

  const saveMutation = useMutation({
    mutationFn: (models: string[]) => saveAutomationModelSelection(targetId, models),
    onSuccess: (saved) => {
      setOrder(saved);
      setDirty(false);
      queryClient.setQueryData(automationKeys.modelSelection(targetId), saved);
      showToast.success("Automation models saved");
    },
    onError: (error) => {
      showToast.error("Could not save models", {
        description: error instanceof Error ? error.message : "Unknown error",
      });
    },
  });

  const addModel = (model: AutomationModel) => {
    setOrder((prev) => (prev.includes(model.id) ? prev : [...prev, model.id]));
    setDirty(true);
  };
  const removeModel = (id: string) => {
    setOrder((prev) => prev.filter((m) => m !== id));
    setDirty(true);
  };
  const handleReorder = (next: string[]) => {
    setOrder(next);
    setDirty(true);
  };

  const isLoading = connectedModels.isLoading || selection.isLoading;
  const loadError = connectedModels.error ?? selection.error;

  return (
    <section className="flex flex-col gap-3">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-base font-semibold text-foreground">Models</h3>
          <p className="text-sm text-muted-foreground">
            The automation user's model list, in priority (fallback) order — top
            runs first.
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {!isLoading && (
            <AddModelButton models={availableToAdd} onSelect={addModel} />
          )}
          {dirty && (
            <Button
              variant="outline"
              size="sm"
              disabled={saveMutation.isPending}
              onClick={() => saveMutation.mutate(order)}
            >
              {saveMutation.isPending ? "Saving…" : "Save"}
            </Button>
          )}
        </div>
      </div>

      {loadError ? (
        <InlineError
          message={loadError instanceof Error ? loadError.message : "Failed to load models"}
          onRetry={() => {
            void connectedModels.refetch();
            void selection.refetch();
          }}
        />
      ) : isLoading ? (
        <div className="py-8 text-center text-sm text-muted-foreground">Loading…</div>
      ) : order.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center text-sm text-muted-foreground">
          {(connectedModels.data?.length ?? 0) === 0
            ? "Connect a provider above to unlock models."
            : "No models selected. Use Add model to pick automation's models."}
        </div>
      ) : (
        <Reorder.Group
          axis="y"
          values={order}
          onReorder={handleReorder}
          className="space-y-2"
          layoutScroll
        >
          {order.map((modelId) => (
            <ModelRow
              key={modelId}
              modelId={modelId}
              model={modelsById.get(modelId)}
              onRemove={() => removeModel(modelId)}
            />
          ))}
        </Reorder.Group>
      )}
    </section>
  );
}

function ModelRow({
  modelId,
  model,
  onRemove,
}: {
  modelId: string;
  model: AutomationModel | undefined;
  onRemove: () => void;
}) {
  const controls = useDragControls();
  const providerId = model?.provider_id ?? modelId.split("/")[0] ?? "unknown";
  const name = model?.name ?? stripProviderPrefix(modelId);

  return (
    <Reorder.Item value={modelId} dragListener={false} dragControls={controls} className="list-none">
      <div className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3">
        <div
          className="shrink-0 cursor-grab touch-none select-none text-muted-foreground/40 active:cursor-grabbing"
          onPointerDown={(e) => controls.start(e)}
        >
          <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
            <circle cx="9" cy="5" r="1.5" /><circle cx="9" cy="12" r="1.5" /><circle cx="9" cy="19" r="1.5" />
            <circle cx="15" cy="5" r="1.5" /><circle cx="15" cy="12" r="1.5" /><circle cx="15" cy="19" r="1.5" />
          </svg>
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold">{name}</div>
          <div className="text-xs text-muted-foreground/60">{formatProvider(providerId)}</div>
        </div>
        <Button
          variant="ghost"
          size="sm"
          onClick={onRemove}
          className="h-8 w-8 shrink-0 p-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
        >
          <HugeiconsIcon icon={Delete02Icon} size={16} />
        </Button>
      </div>
    </Reorder.Item>
  );
}

function AddModelButton({
  models,
  onSelect,
}: {
  models: AutomationModel[];
  onSelect: (model: AutomationModel) => void;
}) {
  const [open, setOpen] = useState(false);

  const groups = useMemo(() => {
    const map = new Map<string, AutomationModel[]>();
    for (const m of models) {
      const provId = m.provider_id ?? "unknown";
      if (!map.has(provId)) map.set(provId, []);
      map.get(provId)!.push(m);
    }
    return Array.from(map.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([provider, items]) => ({
        provider,
        items: items.slice().sort((a, b) => a.name.localeCompare(b.name)),
      }));
  }, [models]);

  return (
    <ModelSelector open={open} onOpenChange={setOpen}>
      <ModelSelectorTrigger render={<Button variant="default" size="sm" />}>
        Add model
      </ModelSelectorTrigger>
      <ModelSelectorContent title="Add a model">
        <ModelSelectorInput placeholder="Search models…" />
        <ModelSelectorList>
          <ModelSelectorEmpty>No connected models available.</ModelSelectorEmpty>
          {groups.map((group, index) => (
            <ModelSelectorGroup key={group.provider} heading={formatProvider(group.provider)}>
              {group.items.map((model) => (
                <ModelSelectorItem
                  key={model.id}
                  searchValue={model.name}
                  onSelect={() => {
                    onSelect(model);
                    setOpen(false);
                  }}
                >
                  <ModelSelectorLogo provider={group.provider} />
                  <ModelSelectorName>{model.name}</ModelSelectorName>
                </ModelSelectorItem>
              ))}
              {index < groups.length - 1 && <ModelSelectorSeparator />}
            </ModelSelectorGroup>
          ))}
        </ModelSelectorList>
      </ModelSelectorContent>
    </ModelSelector>
  );
}

/** Compact badge marking the automation service user in a roster. */
export function ServiceBadge() {
  return (
    <Badge variant="secondary" className="gap-1">
      <HugeiconsIcon icon={SparklesIcon} size={12} />
      Service
    </Badge>
  );
}
