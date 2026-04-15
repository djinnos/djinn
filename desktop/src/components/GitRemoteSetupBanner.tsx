import { useState } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Alert02Icon,
  Cancel01Icon,
  Tick02Icon,
  LinkSquare02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";
import { showToast } from "@/lib/toast";

// Git remote provisioning used to shell out via the Electron host.
// Until the server exposes equivalent HTTP endpoints these are placeholders
// that let the banner continue to render without crashing.
async function checkGitRemote(_projectPath: string): Promise<string | null> {
  return "placeholder"; // assume a remote exists so the banner stays hidden
}

async function setupGitRemote(_projectPath: string, _remoteUrl: string): Promise<string> {
  throw new Error("Git remote setup is not yet available in the web client.");
}

type BannerState = "info" | "running" | "success" | "error";

function isValidGitUrl(url: string): boolean {
  const trimmed = url.trim();
  if (!trimmed) return false;
  // HTTPS: https://github.com/user/repo.git or https://github.com/user/repo
  if (/^https?:\/\/.+\/.+/.test(trimmed)) return true;
  // SSH: git@github.com:user/repo.git
  if (/^git@.+:.+\/.+/.test(trimmed)) return true;
  return false;
}

interface GitRemoteSetupBannerProps {
  projectPath: string;
  onResolved: () => void;
}

export function GitRemoteSetupBanner({ projectPath, onResolved }: GitRemoteSetupBannerProps) {
  const [state, setState] = useState<BannerState>("info");
  const [remoteUrl, setRemoteUrl] = useState("");
  const [errorMessage, setErrorMessage] = useState("");
  const [dismissed, setDismissed] = useState(false);

  if (dismissed) return null;

  const handleSetupRemote = async () => {
    if (!isValidGitUrl(remoteUrl)) {
      setErrorMessage("Please enter a valid HTTPS or SSH git URL.");
      setState("error");
      return;
    }

    setState("running");
    setErrorMessage("");

    try {
      await setupGitRemote(projectPath, remoteUrl.trim());
      setState("success");
      showToast.success("Git remote configured successfully");

      // Auto-dismiss after a short delay
      setTimeout(() => {
        onResolved();
      }, 1500);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setErrorMessage(message);
      setState("error");
    }
  };

  const handleRetry = () => {
    setState("info");
    setErrorMessage("");
  };

  return (
    <Card className="mx-4 border-amber-500/30 bg-amber-500/10">
      <CardContent className="py-4">
        {state === "success" ? (
          <div className="flex items-center gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-emerald-500/20">
              <HugeiconsIcon icon={Tick02Icon} className="size-4 text-emerald-400" />
            </div>
            <p className="text-sm font-medium text-emerald-400">
              Remote configured successfully!
            </p>
          </div>
        ) : (
          <div className="flex flex-col gap-3">
            <div className="flex items-start justify-between gap-3">
              <div className="flex items-start gap-3">
                <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
                  <HugeiconsIcon icon={Alert02Icon} className="size-4 text-amber-400" />
                </div>
                <div className="flex flex-col gap-1">
                  <h3 className="text-sm font-semibold text-amber-200">Git Remote Required</h3>
                  <p className="text-sm text-muted-foreground">
                    Execution requires a git remote to merge completed work.
                  </p>
                </div>
              </div>
              <button
                type="button"
                aria-label="Dismiss Git Remote Setup banner"
                onClick={() => setDismissed(true)}
                className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
              >
                <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
              </button>
            </div>

            <div className="flex flex-col gap-2 pl-11">
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-muted/50 text-xs font-medium">
                  1
                </span>
                <span>Create a new repository on GitHub</span>
                <Button
                  variant="outline"
                  size="sm"
                  className="ml-1 h-6 gap-1 px-2 text-xs"
                  onClick={() => window.open("https://github.com/new", "_blank")}
                >
                  <HugeiconsIcon icon={LinkSquare02Icon} className="size-3" />
                  github.com/new
                </Button>
              </div>

              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-muted/50 text-xs font-medium">
                  2
                </span>
                <span>Paste the repository URL below</span>
              </div>

              <div className="flex items-center gap-2">
                <Input
                  value={remoteUrl}
                  onChange={(e) => setRemoteUrl(e.target.value)}
                  placeholder="https://github.com/you/repo.git"
                  disabled={state === "running"}
                  className="h-8 flex-1 text-sm"
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !state.startsWith("running")) {
                      handleSetupRemote();
                    }
                  }}
                />
                {state === "error" ? (
                  <Button size="sm" className="h-8" onClick={handleRetry}>
                    Retry
                  </Button>
                ) : (
                  <Button
                    size="sm"
                    className="h-8"
                    onClick={handleSetupRemote}
                    disabled={state === "running" || !remoteUrl.trim()}
                  >
                    {state === "running" ? (
                      <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                    ) : (
                      "Setup Remote"
                    )}
                  </Button>
                )}
              </div>

              {state === "error" && errorMessage && (
                <p className="text-sm text-destructive">{errorMessage}</p>
              )}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function useGitRemoteCheck(projectPath: string | undefined) {
  const [hasRemote, setHasRemote] = useState<boolean | null>(null);
  const [checking, setChecking] = useState(false);

  const check = async () => {
    if (!projectPath) {
      setHasRemote(null);
      return;
    }
    setChecking(true);
    try {
      const url = await checkGitRemote(projectPath);
      setHasRemote(url !== null);
    } catch {
      // If the check fails (e.g. not a git repo), don't show the banner
      setHasRemote(true);
    } finally {
      setChecking(false);
    }
  };

  return { hasRemote, checking, check, setHasRemote };
}
