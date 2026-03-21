import { useEffect } from "react";
import { useAuthStore } from "@/stores/authStore";
import { AuthGate } from "./AuthGate";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Field,
  FieldLabel,
  FieldDescription,
  FieldError,
} from "@/components/ui/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Loading02Icon, CheckmarkCircle04Icon, AlertCircleIcon, Folder02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

export default {
  title: "Onboarding",
};

// ---------------------------------------------------------------------------
// AuthGate stories (store seeder pattern)
// ---------------------------------------------------------------------------

function AuthGateState({
  state,
  children,
}: {
  state: Partial<{
    isAuthenticated: boolean;
    isLoading: boolean;
    error: string | null;
    user: { email: string; sub: string } | null;
  }>;
  children?: React.ReactNode;
}) {
  useEffect(() => {
    useAuthStore.setState(state);
  }, [state]);
  return <AuthGate>{children ?? <div className="p-8 text-center text-sm text-muted-foreground">Authenticated app content goes here.</div>}</AuthGate>;
}

export const AuthLoading = {
  name: "AuthGate / Loading",
  render: () => (
    <AuthGateState state={{ isLoading: true, isAuthenticated: false, error: null, user: null }} />
  ),
};

export const AuthSignIn = {
  name: "AuthGate / Sign In Required",
  render: () => (
    <AuthGateState state={{ isLoading: false, isAuthenticated: false, error: null, user: null }} />
  ),
};

export const AuthSignInWithError = {
  name: "AuthGate / Sign In With Error",
  render: () => (
    <AuthGateState
      state={{
        isLoading: false,
        isAuthenticated: false,
        error: "Session expired. Please sign in again.",
        user: null,
      }}
    />
  ),
};

export const AuthAuthenticated = {
  name: "AuthGate / Authenticated",
  render: () => (
    <AuthGateState
      state={{
        isLoading: false,
        isAuthenticated: true,
        error: null,
        user: { email: "dev@djinn.dev", sub: "user-123" },
      }}
    >
      <div className="flex min-h-[200px] items-center justify-center rounded-lg border border-dashed p-8">
        <p className="text-sm text-muted-foreground">
          Protected application content visible after authentication.
        </p>
      </div>
    </AuthGateState>
  ),
};

// ---------------------------------------------------------------------------
// ServerCheckStep visual replicas
// ---------------------------------------------------------------------------

function ServerCheckVisual({
  status,
  errorMessage,
}: {
  status: "checking" | "success" | "error";
  errorMessage?: string;
}) {
  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <div className="flex flex-col items-center gap-4">
        {status === "checking" && (
          <>
            <HugeiconsIcon icon={Loading02Icon} size={48} className="animate-spin text-primary" />
            <div>
              <h2 className="text-xl font-semibold">Connecting to Server</h2>
              <p className="text-sm text-muted-foreground">
                Checking server health...
              </p>
            </div>
          </>
        )}

        {status === "success" && (
          <>
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={48} className="text-green-500" />
            <div>
              <h2 className="text-xl font-semibold">Server Connected</h2>
              <p className="text-sm text-muted-foreground">
                Successfully connected to the Djinn server.
              </p>
            </div>
          </>
        )}

        {status === "error" && (
          <>
            <HugeiconsIcon icon={AlertCircleIcon} size={48} className="text-destructive" />
            <div>
              <h2 className="text-xl font-semibold">Connection Failed</h2>
              <p className="text-sm text-muted-foreground">
                {errorMessage || "Could not connect to the server."}
              </p>
            </div>
            <Button onClick={() => {}} variant="outline">
              Retry Connection
            </Button>
          </>
        )}
      </div>
    </div>
  );
}

export const ServerCheckChecking = {
  name: "ServerCheck / Checking",
  render: () => <ServerCheckVisual status="checking" />,
};

export const ServerCheckSuccess = {
  name: "ServerCheck / Success",
  render: () => <ServerCheckVisual status="success" />,
};

export const ServerCheckError = {
  name: "ServerCheck / Error",
  render: () => (
    <ServerCheckVisual
      status="error"
      errorMessage="Connection refused: server not running on port 8372"
    />
  ),
};

// ---------------------------------------------------------------------------
// ProjectSetupStep visual replicas
// ---------------------------------------------------------------------------

function ProjectSetupVisual({
  selectedPath,
  projectName,
  isRegistering,
  isRegistered,
  error,
}: {
  selectedPath?: string;
  projectName?: string;
  isRegistering?: boolean;
  isRegistered?: boolean;
  error?: string | null;
}) {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input
              placeholder="Select a directory..."
              value={selectedPath || ""}
              readOnly
              className="flex-1"
            />
            <Button variant="secondary" onClick={() => {}}>
              <HugeiconsIcon icon={Folder02Icon} size={16} className="mr-2" />
              Browse
            </Button>
          </div>
          <FieldDescription>
            Choose the root directory for your project.
          </FieldDescription>
        </Field>

        {selectedPath && (
          <Field>
            <FieldLabel>Project Name</FieldLabel>
            <Input
              placeholder="Enter project name..."
              value={projectName || ""}
              readOnly
            />
            <FieldDescription>
              This name will be used to identify your project.
            </FieldDescription>
          </Field>
        )}

        {error && (
          <div className="flex items-center gap-2 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
            <HugeiconsIcon icon={AlertCircleIcon} size={16} className="flex-shrink-0" />
            <span>{error}</span>
          </div>
        )}

        {isRegistered && (
          <div className="flex items-center gap-2 rounded-md bg-green-500/10 p-3 text-sm text-green-600">
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="flex-shrink-0" />
            <span>Project registered successfully!</span>
          </div>
        )}

        <Button
          disabled={!selectedPath || isRegistering || isRegistered}
          className="w-full"
          onClick={() => {}}
        >
          {isRegistering ? (
            <>
              <HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />
              Registering...
            </>
          ) : isRegistered ? (
            <>
              <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="mr-2" />
              Registered
            </>
          ) : (
            "Register Project"
          )}
        </Button>
      </div>
    </div>
  );
}

export const ProjectSetupEmpty = {
  name: "ProjectSetup / Empty",
  render: () => <ProjectSetupVisual />,
};

export const ProjectSetupDirectorySelected = {
  name: "ProjectSetup / Directory Selected",
  render: () => (
    <ProjectSetupVisual
      selectedPath="/home/dev/projects/my-app"
      projectName="my-app"
    />
  ),
};

export const ProjectSetupRegistering = {
  name: "ProjectSetup / Registering",
  render: () => (
    <ProjectSetupVisual
      selectedPath="/home/dev/projects/my-app"
      projectName="my-app"
      isRegistering
    />
  ),
};

export const ProjectSetupRegistered = {
  name: "ProjectSetup / Registered",
  render: () => (
    <ProjectSetupVisual
      selectedPath="/home/dev/projects/my-app"
      projectName="my-app"
      isRegistered
    />
  ),
};

export const ProjectSetupError = {
  name: "ProjectSetup / Error",
  render: () => (
    <ProjectSetupVisual
      selectedPath="/home/dev/projects/my-app"
      projectName="my-app"
      error="Failed to register project: directory not found"
    />
  ),
};

// ---------------------------------------------------------------------------
// ProviderSetupStep visual replicas
// ---------------------------------------------------------------------------

function ProviderSetupVisual({
  variant,
}: {
  variant:
    | "loading"
    | "loadError"
    | "selectProvider"
    | "apiKeyEntry"
    | "apiKeyValidated"
    | "apiKeyError"
    | "oauthProvider"
    | "oauthInProgress";
}) {
  if (variant === "loading") {
    return (
      <div className="flex flex-col items-center gap-4 text-center">
        <HugeiconsIcon icon={Loading02Icon} size={32} className="animate-spin text-primary" />
        <p className="text-sm text-muted-foreground">Loading providers...</p>
      </div>
    );
  }

  if (variant === "loadError") {
    return (
      <div className="flex flex-col items-center gap-4 text-center">
        <HugeiconsIcon icon={AlertCircleIcon} size={48} className="text-destructive" />
        <div>
          <h2 className="text-xl font-semibold">Failed to Load Providers</h2>
          <p className="text-sm text-muted-foreground">
            Network error: could not reach the server
          </p>
        </div>
        <Button onClick={() => {}} variant="outline">
          Retry
        </Button>
      </div>
    );
  }

  const showApiKey = [
    "apiKeyEntry",
    "apiKeyValidated",
    "apiKeyError",
    "oauthProvider",
    "oauthInProgress",
  ].includes(variant);
  const isOAuth = variant === "oauthProvider" || variant === "oauthInProgress";
  const selectedName = isOAuth ? "Anthropic" : "OpenAI";

  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select value={selectedName.toLowerCase()} onValueChange={() => {}}>
            <SelectTrigger className="w-full">
              <SelectValue placeholder="Select a provider" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="openai">OpenAI</SelectItem>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="google">Google AI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        {isOAuth && (
          <Button
            onClick={() => {}}
            disabled={variant === "oauthInProgress"}
            className="w-full"
          >
            {variant === "oauthInProgress" ? (
              <>
                <HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />
                Waiting for browser...
              </>
            ) : (
              "Connect with OAuth"
            )}
          </Button>
        )}

        {isOAuth && (
          <div className="flex items-center gap-3 text-xs text-muted-foreground">
            <div className="h-px flex-1 bg-border" />
            <span>or enter an API key</span>
            <div className="h-px flex-1 bg-border" />
          </div>
        )}

        {showApiKey && (
          <Field>
            <FieldLabel>API Key</FieldLabel>
            <div className="flex gap-2">
              <Input
                type="password"
                placeholder="Enter your API key"
                value={
                  variant === "apiKeyEntry" || variant === "oauthProvider" || variant === "oauthInProgress"
                    ? ""
                    : "sk-proj-abc123...xyz789"
                }
                readOnly
                className="flex-1"
              />
              <Button
                variant="secondary"
                disabled={
                  variant === "apiKeyEntry" ||
                  variant === "apiKeyValidated" ||
                  variant === "oauthProvider" ||
                  variant === "oauthInProgress"
                }
                onClick={() => {}}
              >
                Validate
              </Button>
            </div>
            <FieldDescription>
              Your API key will be securely stored and never shared.
            </FieldDescription>
            {variant === "apiKeyValidated" && (
              <div className="flex items-center gap-2 text-sm text-green-500">
                <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} />
                <span>API key is valid</span>
              </div>
            )}
            {variant === "apiKeyError" && (
              <FieldError>Invalid API key: authentication failed</FieldError>
            )}
          </Field>
        )}

        {!isOAuth && showApiKey && (
          <Button
            disabled={variant === "apiKeyEntry"}
            className="w-full"
            onClick={() => {}}
          >
            Continue
          </Button>
        )}
      </div>
    </div>
  );
}

export const ProviderSetupLoading = {
  name: "ProviderSetup / Loading",
  render: () => <ProviderSetupVisual variant="loading" />,
};

export const ProviderSetupLoadError = {
  name: "ProviderSetup / Load Error",
  render: () => <ProviderSetupVisual variant="loadError" />,
};

export const ProviderSetupSelectProvider = {
  name: "ProviderSetup / Select Provider",
  render: () => <ProviderSetupVisual variant="selectProvider" />,
};

export const ProviderSetupApiKeyEntry = {
  name: "ProviderSetup / API Key Entry",
  render: () => <ProviderSetupVisual variant="apiKeyEntry" />,
};

export const ProviderSetupApiKeyValidated = {
  name: "ProviderSetup / API Key Validated",
  render: () => <ProviderSetupVisual variant="apiKeyValidated" />,
};

export const ProviderSetupApiKeyError = {
  name: "ProviderSetup / API Key Error",
  render: () => <ProviderSetupVisual variant="apiKeyError" />,
};

export const ProviderSetupOAuth = {
  name: "ProviderSetup / OAuth Provider",
  render: () => <ProviderSetupVisual variant="oauthProvider" />,
};

export const ProviderSetupOAuthInProgress = {
  name: "ProviderSetup / OAuth In Progress",
  render: () => <ProviderSetupVisual variant="oauthInProgress" />,
};
