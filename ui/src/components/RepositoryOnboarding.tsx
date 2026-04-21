import { useState } from 'react';
import { GithubIcon, PlusSignIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';

import logoSvg from '@/assets/logo.svg';
import { Button } from '@/components/ui/button';
import { AddProjectFromGithubDialog } from '@/components/AddProjectFromGithubDialog';
import { fetchProjects } from '@/api/server';
import { projectStore } from '@/stores/projectStore';
import { useProjectGateStore } from '@/stores/projectGateStore';

/**
 * Onboarding gate rendered when no repository has been added to the
 * deployment yet. Mirrors `ProviderOnboarding` / `ModelOnboarding`: Djinn
 * glow logo + a single primary action that opens the GitHub repo picker.
 */
export function RepositoryOnboarding() {
  const { refresh } = useProjectGateStore();
  const [dialogOpen, setDialogOpen] = useState(false);

  const handleAdded = async () => {
    // Populate the project list store so the app lands in a ready state
    // once the gate closes, then flip the gate.
    try {
      const projects = await fetchProjects();
      projectStore.getState().setProjects(projects);
    } catch {
      // If the post-add refresh fails, still flip the gate — the user can
      // retry from inside the app.
    }
    await refresh();
  };

  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background text-foreground px-6 py-12">
      <div className="flex w-full max-w-xl flex-col items-center gap-10">
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-20 w-20 rounded-full bg-purple-400/40"
            style={{ filter: 'blur(50px)' }}
          />
          <img
            src={logoSvg}
            alt="Djinn"
            className="relative h-20 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]"
          />
        </div>

        <div className="text-center space-y-2">
          <h2 className="text-2xl font-semibold">Add a repository</h2>
          <p className="text-base text-muted-foreground">
            Djinn needs at least one GitHub repository to manage tasks, epics, and agents.
          </p>
        </div>

        <div className="relative flex w-full flex-col gap-4 overflow-hidden rounded-2xl border border-primary/40 bg-gradient-to-br from-primary/[0.06] to-transparent p-7">
          <div className="pointer-events-none absolute -right-8 -top-8 h-28 w-28 rounded-full bg-primary/20 blur-3xl" />
          <div className="pointer-events-none absolute -left-6 -bottom-6 h-20 w-20 rounded-full bg-primary/10 blur-3xl" />

          <div className="flex items-center gap-3">
            <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
              <HugeiconsIcon icon={GithubIcon} size={20} className="text-primary" />
            </div>
            <div>
              <h3 className="text-base font-semibold text-foreground">GitHub</h3>
              <p className="text-xs text-muted-foreground">
                Pick a repository from your connected GitHub org
              </p>
            </div>
          </div>

          <p className="text-sm leading-relaxed text-muted-foreground">
            Djinn clones the repository into its managed volume and starts tracking its tasks,
            epics, and branches. You can add more repositories later from the Repositories page.
          </p>

          <Button size="lg" className="w-full text-sm" onClick={() => setDialogOpen(true)}>
            <HugeiconsIcon icon={PlusSignIcon} size={16} />
            Browse repositories
          </Button>
        </div>
      </div>

      <AddProjectFromGithubDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        onAdded={() => void handleAdded()}
      />
    </main>
  );
}
