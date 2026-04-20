/**
 * In-UI GitHub App installation picker.
 *
 * Rendered by `AuthGate` when `setupStatus.appCredentialsConfigured === true`
 * and `setupStatus.needsAppInstall === true` — i.e., the operator dropped
 * the `djinn-github-app` Secret but didn't pre-bind the deployment to a
 * specific installation via `GITHUB_INSTALLATION_ID`. This component lets
 * them pick from `GET /app/installations` instead.
 *
 * The env-binding override stays as the CI/automation path; this UI is
 * never reached when env binding is present (server returns
 * `needs_app_install: false`).
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import {
  fetchInstallations,
  selectInstallation,
  type InstallationSummary,
} from "@/api/auth";

export const INSTALLATIONS_QUERY_KEY = ["github", "installations"] as const;
export const SETUP_STATUS_QUERY_KEY = ["auth", "setup-status"] as const;

export function InstallationPicker() {
  const queryClient = useQueryClient();

  const {
    data: installations,
    isLoading,
    isError,
    error,
    refetch,
  } = useQuery({
    queryKey: INSTALLATIONS_QUERY_KEY,
    queryFn: fetchInstallations,
    retry: false,
    staleTime: 30_000,
  });

  const select = useMutation({
    mutationFn: (id: number) => selectInstallation(id),
    onSuccess: () => {
      // Re-pull `/setup/status` so the gate transitions to sign-in.
      queryClient.invalidateQueries({ queryKey: SETUP_STATUS_QUERY_KEY });
    },
  });

  if (isLoading) {
    return (
      <div className="flex w-full flex-col items-center gap-3 text-center">
        <Spinner />
        <p className="text-sm text-muted-foreground">Loading installations...</p>
      </div>
    );
  }

  if (isError) {
    return (
      <PickerErrorPanel
        message={
          error instanceof Error
            ? error.message
            : "Could not fetch GitHub installations."
        }
        onRetry={() => refetch()}
      />
    );
  }

  const list = installations ?? [];

  if (list.length === 0) {
    return <EmptyInstallationsPanel />;
  }

  return (
    <div className="w-full space-y-4 text-left">
      <div className="space-y-2 text-center">
        <h2 className="text-lg font-semibold">Pick a GitHub installation</h2>
        <p className="text-sm text-muted-foreground">
          Choose which org or account this Djinn deployment should bind to.
          You can re-bind later by an operator running a database migration.
        </p>
      </div>

      <ul className="flex flex-col gap-2">
        {list.map((inst) => (
          <li key={inst.installationId}>
            <InstallationRow
              installation={inst}
              onSelect={() => select.mutate(inst.installationId)}
              disabled={select.isPending}
              isSubmitting={
                select.isPending && select.variables === inst.installationId
              }
            />
          </li>
        ))}
      </ul>

      {select.isError && (
        <p className="text-center text-sm text-destructive">
          {select.error instanceof Error
            ? select.error.message
            : "Failed to bind installation."}
        </p>
      )}
    </div>
  );
}

function InstallationRow({
  installation,
  onSelect,
  disabled,
  isSubmitting,
}: {
  installation: InstallationSummary;
  onSelect: () => void;
  disabled: boolean;
  isSubmitting: boolean;
}) {
  const scopeHint =
    installation.repositorySelection === "selected"
      ? "limited to selected repos"
      : "all current and future repos";
  const accountTypeLabel =
    installation.accountType === "Organization" ? "Organization" : "User";

  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      className="group flex w-full items-center justify-between rounded-lg border border-border/60 bg-card/50 px-4 py-3 text-left transition hover:border-purple-400/60 hover:bg-card/80 disabled:cursor-not-allowed disabled:opacity-60"
    >
      <div className="flex flex-col gap-0.5">
        <span className="font-medium text-foreground">
          {installation.accountLogin || `Installation #${installation.installationId}`}
        </span>
        <span className="text-xs text-muted-foreground">
          {accountTypeLabel} · {scopeHint}
        </span>
      </div>
      <div className="flex items-center gap-3 text-sm text-muted-foreground group-hover:text-foreground">
        {isSubmitting ? <Spinner /> : <span>Use this installation</span>}
      </div>
    </button>
  );
}

function PickerErrorPanel({
  message,
  onRetry,
}: {
  message: string;
  onRetry: () => void;
}) {
  return (
    <div className="w-full space-y-3 text-center">
      <h2 className="text-lg font-semibold">Could not load installations</h2>
      <p className="text-sm text-muted-foreground">{message}</p>
      <Button onClick={onRetry} variant="outline" size="sm">
        Try again
      </Button>
    </div>
  );
}

function EmptyInstallationsPanel() {
  return (
    <div className="w-full space-y-3 text-center">
      <h2 className="text-lg font-semibold">No installations yet</h2>
      <p className="text-sm text-muted-foreground">
        The Djinn GitHub App isn't installed on any organization yet. Install
        it on the org you want to bind, then come back to this screen.
      </p>
      <a
        href="https://github.com/settings/installations"
        target="_blank"
        rel="noopener noreferrer"
        className="inline-block text-sm text-purple-300 underline-offset-4 hover:text-purple-200 hover:underline"
      >
        Install the Djinn App on a GitHub org
      </a>
    </div>
  );
}
