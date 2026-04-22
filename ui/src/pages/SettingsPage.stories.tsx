import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

/* -------------------------------------------------------------------------- */
/*  Visual-replica approach                                                    */
/*                                                                            */
/*  The real SettingsPage calls hooks that reach out to a live backend.        */
/*  Rather than wrestling with module-level mocks that can't vary per story,   */
/*  we build lightweight replicas of the shell + each sub-view so every story  */
/*  gets deterministic, self-contained markup.                                 */
/* -------------------------------------------------------------------------- */

// -- Shared types & constants ------------------------------------------------

type SettingsCategory = "providers" | "projects" | "agents";

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: "providers", label: "Providers" },
  { key: "projects", label: "Projects" },
  { key: "agents", label: "Agents" },
];

// -- Settings shell ----------------------------------------------------------

function SettingsShell({
  activeCategory,
  children,
}: {
  activeCategory: SettingsCategory;
  children: React.ReactNode;
}) {
  return (
    <MemoryRouter initialEntries={[`/settings/${activeCategory}`]}>
      <div className="flex h-full flex-col overflow-hidden p-6">
        <div className="mb-6 shrink-0">
          <h1 className="text-2xl font-bold text-foreground">Settings</h1>
          <p className="mt-1 text-muted-foreground">
            Configure your workspace preferences
          </p>
        </div>

        <div className="flex min-h-0 flex-1 flex-col gap-6 md:flex-row">
          <aside className="md:w-56 md:shrink-0">
            <nav className="flex flex-row gap-2 overflow-x-auto md:flex-col md:overflow-visible">
              {categories.map((item) => (
                <span
                  key={item.key}
                  className={cn(
                    "rounded-md px-3 py-2 text-sm transition-colors cursor-default",
                    item.key === activeCategory
                      ? "bg-primary text-primary-foreground"
                      : "text-muted-foreground hover:bg-muted hover:text-foreground",
                  )}
                >
                  {item.label}
                </span>
              ))}
            </nav>
          </aside>

          <section className="min-h-0 min-w-0 flex-1 flex flex-col overflow-y-auto pb-6">
            {children}
          </section>
        </div>
      </div>
    </MemoryRouter>
  );
}

// -- Providers replica -------------------------------------------------------

interface ProviderEntry {
  id: string;
  name: string;
  description: string;
}

function ProvidersReplica({
  configured,
}: {
  configured: ProviderEntry[];
}) {
  return (
    <div className="flex flex-col gap-4 flex-1 min-h-0">
      <div className="flex items-center justify-between shrink-0">
        <h2 className="text-lg font-semibold">Configured Providers</h2>
        <Button>Add Provider</Button>
      </div>

      <div className="space-y-2 shrink-0">
        {configured.map((provider) => (
          <div
            key={provider.id}
            className="flex items-center justify-between rounded-lg border border-border bg-card p-4"
          >
            <div>
              <p className="font-medium">{provider.name}</p>
              <p className="text-xs text-muted-foreground">Configured</p>
            </div>
            <Button variant="destructive" size="sm">
              Remove
            </Button>
          </div>
        ))}
        {configured.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No providers configured yet.
          </p>
        )}
      </div>
    </div>
  );
}

// -- Projects replica --------------------------------------------------------

interface ProjectEntry {
  id: string;
  name: string;
  path: string;
  branch: string;
  auto_merge: boolean;
}

function ProjectsReplica({
  projects,
  expandedId,
}: {
  projects: ProjectEntry[];
  expandedId?: string;
}) {
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Projects</h2>
          <p className="text-sm text-muted-foreground">
            Registered projects and defaults.
          </p>
        </div>
        <Button>Add Project</Button>
      </div>

      {projects.length === 0 ? (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          No projects registered yet.
        </div>
      ) : (
        <div className="space-y-2">
          {projects.map((project) => {
            const expanded = expandedId === project.id;
            return (
              <div
                key={project.id}
                className="rounded-lg border border-border bg-card p-4 space-y-3"
              >
                <div className="flex items-center justify-between gap-4">
                  <button className="min-w-0 text-left cursor-pointer">
                    <div className="flex items-center gap-2">
                      <p className="font-medium">{project.name}</p>
                    </div>
                    <p className="truncate text-xs text-muted-foreground">
                      {project.path}
                    </p>
                  </button>
                  <Button variant="destructive" size="sm">
                    Remove
                  </Button>
                </div>
                {expanded && (
                  <div className="grid gap-3 pt-2 border-t border-border">
                    <div className="space-y-1">
                      <p className="text-sm font-medium">Target branch</p>
                      <Input defaultValue={project.branch} placeholder="main" />
                    </div>
                    <div className="flex items-center justify-between">
                      <p className="text-sm font-medium">Auto-merge</p>
                      <input
                        type="checkbox"
                        className="h-4 w-4"
                        defaultChecked={project.auto_merge}
                      />
                    </div>
                    <p className="border-t border-border pt-3 text-xs text-muted-foreground">
                      Setup and verification commands are configured via the project's{" "}
                      <code className="rounded bg-muted px-1">environment config</code>
                    </p>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

// -- Agents replica ----------------------------------------------------------

const ROLE_LABELS: Record<string, string> = {
  worker: "W",
  reviewer: "R",
  lead: "L",
  planner: "P",
};

const ROLE_FULL_LABELS: Record<string, string> = {
  worker: "Worker",
  reviewer: "Reviewer",
  lead: "Lead",
  planner: "Planner",
};

const ALL_ROLES = [
  "worker",
  "reviewer",
  "lead",
  "planner",
];

interface ModelEntry {
  model: string;
  provider: string;
  enabledRoles: string[];
  max_concurrent: number;
}

function AgentsReplica({ models }: { models: ModelEntry[] }) {
  return (
    <div className="space-y-6">
      <div className="space-y-4">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="text-lg font-semibold">Model Configuration</h3>
            <p className="text-sm text-muted-foreground">
              Add models, set session limits, and toggle which agents can use
              each model. Drag to reorder priority.
            </p>
          </div>
        </div>

        {/* Add Model */}
        <Input placeholder="Search models..." />

        {/* Role Legend */}
        <div className="flex items-center gap-4 text-xs text-muted-foreground">
          {ALL_ROLES.map((role) => (
            <span key={role}>
              <span className="font-semibold text-foreground">
                {ROLE_LABELS[role]}
              </span>
              {" = "}
              {ROLE_FULL_LABELS[role]}
            </span>
          ))}
        </div>

        {/* Model List */}
        {models.length === 0 ? (
          <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
            No models configured. Add models from connected providers above.
          </div>
        ) : (
          <div className="space-y-2">
            {models.map((entry, index) => (
              <div key={`${entry.provider}-${entry.model}-${index}`}>
                <div className="flex items-center gap-3 rounded-md border bg-card p-3">
                  {/* Drag Handle */}
                  <div className="flex flex-col text-muted-foreground cursor-grab shrink-0">
                    <svg
                      xmlns="http://www.w3.org/2000/svg"
                      width="16"
                      height="16"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <circle cx="9" cy="12" r="1" />
                      <circle cx="9" cy="5" r="1" />
                      <circle cx="9" cy="19" r="1" />
                      <circle cx="15" cy="12" r="1" />
                      <circle cx="15" cy="5" r="1" />
                      <circle cx="15" cy="19" r="1" />
                    </svg>
                  </div>

                  {/* Model Info */}
                  <div className="min-w-0 flex-1">
                    <div className="font-medium truncate">{entry.model}</div>
                    <div className="text-xs text-muted-foreground">
                      {entry.provider}
                    </div>
                  </div>

                  {/* Agent Role Toggles */}
                  <div className="flex items-center gap-1 shrink-0">
                    {ALL_ROLES.map((role) => {
                      const enabled = entry.enabledRoles.includes(role);
                      return (
                        <span
                          key={role}
                          className={cn(
                            "flex h-7 min-w-[28px] items-center justify-center rounded px-1.5 text-xs font-semibold",
                            enabled
                              ? "bg-primary text-primary-foreground"
                              : "bg-muted text-muted-foreground",
                          )}
                        >
                          {ROLE_LABELS[role]}
                        </span>
                      );
                    })}
                  </div>

                  {/* Max Sessions */}
                  <div className="flex items-center gap-2 shrink-0">
                    <span className="text-xs text-muted-foreground whitespace-nowrap">
                      Max:
                    </span>
                    <Input
                      type="number"
                      min={1}
                      max={10}
                      defaultValue={entry.max_concurrent}
                      className="w-16 h-8"
                    />
                  </div>

                  {/* Remove */}
                  <Button variant="ghost" size="sm" className="h-8 w-8 p-0 shrink-0">
                    <svg
                      xmlns="http://www.w3.org/2000/svg"
                      width="16"
                      height="16"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <path d="M18 6 6 18" />
                      <path d="m6 6 12 12" />
                    </svg>
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// -- Mock data ---------------------------------------------------------------

const sampleProviders: ProviderEntry[] = [
  { id: "anthropic", name: "Anthropic", description: "Claude models" },
  { id: "openai", name: "OpenAI", description: "GPT models" },
];

const sampleProjects: ProjectEntry[] = [
  {
    id: "proj-1",
    name: "desktop",
    path: "/home/fernando/git/djinnos/desktop",
    branch: "main",
    auto_merge: false,
  },
  {
    id: "proj-2",
    name: "api-server",
    path: "/home/fernando/git/djinnos/api",
    branch: "develop",
    auto_merge: true,
  },
];

const sampleModels: ModelEntry[] = [
  {
    model: "claude-sonnet-4-20250514",
    provider: "anthropic",
    enabledRoles: ["worker", "reviewer", "planner"],
    max_concurrent: 3,
  },
  {
    model: "gpt-4o",
    provider: "openai",
    enabledRoles: ["worker", "lead"],
    max_concurrent: 2,
  },
];

// -- Story wrapper -----------------------------------------------------------

interface SettingsStoryProps {
  activeCategory: SettingsCategory;
  providers?: ProviderEntry[];
  projects?: ProjectEntry[];
  expandedProjectId?: string;
  models?: ModelEntry[];
}

function SettingsStory({
  activeCategory,
  providers = [],
  projects = [],
  expandedProjectId,
  models = [],
}: SettingsStoryProps) {
  return (
    <SettingsShell activeCategory={activeCategory}>
      {activeCategory === "providers" && (
        <ProvidersReplica configured={providers} />
      )}
      {activeCategory === "projects" && (
        <ProjectsReplica projects={projects} expandedId={expandedProjectId} />
      )}
      {activeCategory === "agents" && <AgentsReplica models={models} />}
    </SettingsShell>
  );
}

// -- Storybook meta ----------------------------------------------------------

const meta: Meta<typeof SettingsStory> = {
  title: "Pages/Settings",
  component: SettingsStory,
  parameters: { layout: "fullscreen" },
};

export default meta;
type Story = StoryObj<typeof SettingsStory>;

// -- Stories -----------------------------------------------------------------

/** Providers tab with two configured providers (Anthropic + OpenAI). */
export const ProvidersView: Story = {
  args: {
    activeCategory: "providers",
    providers: sampleProviders,
  },
};

/** Providers tab with no providers configured. */
export const ProvidersEmpty: Story = {
  args: {
    activeCategory: "providers",
    providers: [],
  },
};

/** Projects tab showing two registered projects (collapsed). */
export const ProjectsView: Story = {
  args: {
    activeCategory: "projects",
    projects: sampleProjects,
  },
};

/** Projects tab with the first project expanded, showing branch & auto-merge settings. */
export const ProjectsExpanded: Story = {
  args: {
    activeCategory: "projects",
    projects: sampleProjects,
    expandedProjectId: "proj-1",
  },
};

/** Agents tab with two models configured and role toggles visible. */
export const AgentsView: Story = {
  args: {
    activeCategory: "agents",
    models: sampleModels,
  },
};
