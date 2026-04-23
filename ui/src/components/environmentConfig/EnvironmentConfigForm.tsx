/**
 * EnvironmentConfigForm — the form-based editor for a single project's
 * `environment_config`. Panes: Base image, Workspaces, System packages,
 * Env vars, Lifecycle hooks, Verification.
 *
 * Workspaces are the primary unit of toolchain config: each workspace
 * pins its own language + toolchain/version, and the image-builder
 * aggregates the union across workspaces (e.g. `NODE_VERSIONS="20 22"`).
 * Per-language image knobs (`components`, `scip_indexer`) are synthesized
 * from the workspace list on save — see `ensureLanguageEnabled`.
 *
 * The form binds against the normalized `EnvironmentConfig` shape from
 * `@/api/environmentConfig`; any field unknown to the form passes through
 * untouched so manual JSON edits round-trip cleanly.
 */
import { HugeiconsIcon } from "@hugeicons/react";
import { Delete02Icon, PlusSignIcon } from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  type EnvironmentConfig,
  type LanguageKey,
  type Languages,
  type Workspace,
  LANGUAGE_KEYS,
} from "@/api/environmentConfig";
import { HookCommandList } from "@/components/environmentConfig/HookCommandList";

interface Props {
  config: EnvironmentConfig;
  onChange: (next: EnvironmentConfig) => void;
}

export function EnvironmentConfigForm({ config, onChange }: Props) {
  return (
    <div className="flex flex-col gap-8">
      <WorkspacesSection config={config} onChange={onChange} />
      <SystemPackagesSection config={config} onChange={onChange} />
      <EnvVarsSection config={config} onChange={onChange} />
      <LifecycleSection config={config} onChange={onChange} />
      <VerificationSection config={config} onChange={onChange} />
    </div>
  );
}

// Base image distro/variant is intentionally not in the form. Djinn's
// image runs verification only (clippy/check/test/lint) — it doesn't
// ship anything, so libc flavor and package-manager choice are a detail
// almost no one should be reasoning about. The `base` field still exists
// on `EnvironmentConfig` and round-trips through the Raw JSON editor for
// the rare case where someone genuinely needs alpine.

// ── Language helpers (consumed by the Workspaces section) ──────────────

const LANGUAGE_LABELS: Record<LanguageKey, string> = {
  rust: "Rust",
  node: "Node",
  python: "Python",
  go: "Go",
  java: "Java",
  ruby: "Ruby",
  dotnet: ".NET",
  clang: "Clang",
};

// Seed for `config.languages.*` when a workspace enables a language.
// `default_*` values are fallbacks for workspaces that leave `version`
// blank — the image-builder unions them with per-workspace pins and
// emits install lines for the full set.
function defaultLanguageBlock(lang: LanguageKey): NonNullable<Languages[LanguageKey]> {
  switch (lang) {
    case "rust":
      return { default_toolchain: "stable" };
    case "node":
      return { default_version: "22", default_package_manager: "pnpm" };
    case "python":
      return { default_version: "3.12" };
    case "go":
      return { default_version: "1.22" };
    case "java":
      return { default_version: "21" };
    case "ruby":
      return { default_version: "3.3" };
    case "dotnet":
      return { default_version: "8.0" };
    case "clang":
      return { default_version: "17" };
  }
}

function InlineField({
  label,
  value,
  onChange,
  placeholder,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <div className="flex flex-col gap-1">
      <Label className="text-xs text-muted-foreground">{label}</Label>
      <Input
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="font-mono text-xs"
      />
    </div>
  );
}

// ── Workspaces ──────────────────────────────────────────────────────────

function WorkspacesSection({ config, onChange }: Props) {
  // A workspace pinned to language X is meaningless unless the image
  // actually installs X, so any path that sets/changes a workspace's
  // language auto-enables that language in config.languages (with the
  // same defaults the toggle uses). We never auto-disable — removing
  // the last Go workspace doesn't nuke Go config, since the user may
  // want it installed for scripts or tests.
  const ensureLanguageEnabled = (langs: Languages, lang: string): Languages => {
    if (!LANGUAGE_KEYS.includes(lang as LanguageKey)) return langs;
    if (langs[lang as LanguageKey] !== undefined) return langs;
    return { ...langs, [lang]: defaultLanguageBlock(lang as LanguageKey) };
  };

  const updateAt = (idx: number, patch: Partial<Workspace>) => {
    const nextWorkspaces = config.workspaces.slice();
    nextWorkspaces[idx] = { ...nextWorkspaces[idx], ...patch };
    const nextLanguages =
      typeof patch.language === "string"
        ? ensureLanguageEnabled(config.languages, patch.language)
        : config.languages;
    onChange({ ...config, workspaces: nextWorkspaces, languages: nextLanguages });
  };

  const remove = (idx: number) => {
    const next = config.workspaces.slice();
    next.splice(idx, 1);
    onChange({ ...config, workspaces: next });
  };

  const add = () => {
    onChange({
      ...config,
      workspaces: [...config.workspaces, { slug: "", root: "", language: "rust" }],
      languages: ensureLanguageEnabled(config.languages, "rust"),
    });
  };

  return (
    <Section
      title="Workspaces"
      description="Each workspace is a manifest directory (Cargo.toml, package.json, etc.) with its own toolchain. The image installs the union of all pinned versions; the language picker controls which fields apply."
    >
      <div className="flex flex-col gap-3">
        {config.workspaces.length === 0 && (
          <p className="text-xs text-muted-foreground">
            No workspaces configured. Detection populates these on first boot.
          </p>
        )}
        {config.workspaces.map((ws, idx) => (
          <WorkspaceCard
            key={idx}
            workspace={ws}
            onChange={(patch) => updateAt(idx, patch)}
            onRemove={() => remove(idx)}
          />
        ))}
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit gap-1.5 text-xs"
          onClick={add}
        >
          <HugeiconsIcon icon={PlusSignIcon} size={12} />
          Add workspace
        </Button>
      </div>
    </Section>
  );
}

function WorkspaceCard({
  workspace,
  onChange,
  onRemove,
}: {
  workspace: Workspace;
  onChange: (patch: Partial<Workspace>) => void;
  onRemove: () => void;
}) {
  const isRust = workspace.language === "rust";
  const isNode = workspace.language === "node";

  // When the language changes, drop field values that no longer apply so
  // the saved JSON stays minimal and validation-friendly. Rust uses
  // `toolchain`, Node uses `version` + `package_manager`, everything else
  // uses `version` only.
  const handleLanguageChange = (lang: string | null) => {
    if (!lang) return;
    onChange({
      language: lang,
      toolchain: lang === "rust" ? (workspace.toolchain ?? null) : null,
      version: lang === "rust" ? null : (workspace.version ?? null),
      package_manager: lang === "node" ? (workspace.package_manager ?? null) : null,
    });
  };

  const headerLabel = workspace.slug || workspace.root || "new workspace";

  return (
    <div className="rounded-md border bg-background/30">
      <div className="flex items-center justify-between gap-2 border-b border-border/40 px-3 py-2">
        <span className="font-mono text-xs text-muted-foreground">{headerLabel}</span>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
          onClick={onRemove}
        >
          <HugeiconsIcon icon={Delete02Icon} size={12} />
        </Button>
      </div>
      <div className="flex flex-col gap-2.5 px-3 py-3">
        <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2">
          <InlineField
            label="Slug"
            value={workspace.slug}
            onChange={(v) => onChange({ slug: v })}
            placeholder="backend"
          />
          <InlineField
            label="Root"
            value={workspace.root}
            onChange={(v) => onChange({ root: v })}
            placeholder="server/"
          />
        </div>
        <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2">
          <div className="flex flex-col gap-1">
            <Label className="text-xs text-muted-foreground">Language</Label>
            <Select value={workspace.language} onValueChange={handleLanguageChange}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {LANGUAGE_KEYS.map((lang) => (
                  <SelectItem key={lang} value={lang}>
                    {LANGUAGE_LABELS[lang]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          {isRust ? (
            <InlineField
              label="Toolchain"
              value={workspace.toolchain ?? ""}
              onChange={(v) => onChange({ toolchain: v || null })}
              placeholder="stable"
            />
          ) : (
            <InlineField
              label="Version"
              value={workspace.version ?? ""}
              onChange={(v) => onChange({ version: v || null })}
              placeholder="latest"
            />
          )}
        </div>
        {isNode && (
          <InlineField
            label="Package manager"
            value={workspace.package_manager ?? ""}
            onChange={(v) => onChange({ package_manager: v || null })}
            placeholder="pnpm"
          />
        )}
      </div>
    </div>
  );
}

// ── System packages ─────────────────────────────────────────────────────

function SystemPackagesSection({ config, onChange }: Props) {
  return (
    <Section
      title="System packages"
      description="One apt package name per line. For anything that isn't in apt (curl installs like `protoc`, binary downloads, etc.), use a `post_build` lifecycle hook so it bakes into the image."
    >
      <Textarea
        value={config.system_packages.join("\n")}
        onChange={(e) =>
          onChange({
            ...config,
            system_packages: e.target.value
              .split("\n")
              .map((s) => s.trim())
              .filter(Boolean),
          })
        }
        placeholder="postgresql-client"
        className="min-h-[100px] font-mono text-xs"
      />
    </Section>
  );
}

// ── Env vars ────────────────────────────────────────────────────────────

function EnvVarsSection({ config, onChange }: Props) {
  const entries = Object.entries(config.env);

  const updateKey = (oldKey: string, newKey: string) => {
    if (!newKey || newKey === oldKey) return;
    const next: Record<string, string> = {};
    for (const [k, v] of entries) next[k === oldKey ? newKey : k] = v;
    onChange({ ...config, env: next });
  };
  const updateValue = (key: string, value: string) => {
    onChange({ ...config, env: { ...config.env, [key]: value } });
  };
  const remove = (key: string) => {
    const next = { ...config.env };
    delete next[key];
    onChange({ ...config, env: next });
  };
  const add = () => {
    let key = "VAR";
    let i = 1;
    while (key in config.env) {
      i += 1;
      key = `VAR${i}`;
    }
    onChange({ ...config, env: { ...config.env, [key]: "" } });
  };

  return (
    <Section
      title="Environment variables"
      description="Set via `ENV` lines in the generated Dockerfile. Keys must match `[A-Za-z_][A-Za-z0-9_]*`."
    >
      <div className="flex flex-col gap-2">
        {entries.length === 0 && (
          <p className="text-xs text-muted-foreground">No env vars configured.</p>
        )}
        {entries.map(([key, value]) => (
          <div key={key} className="flex items-center gap-1.5">
            <Input
              defaultValue={key}
              onBlur={(e) => updateKey(key, e.target.value.trim())}
              className="w-48 font-mono text-xs"
            />
            <Input
              value={value}
              onChange={(e) => updateValue(key, e.target.value)}
              className="flex-1 font-mono text-xs"
            />
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
              onClick={() => remove(key)}
            >
              <HugeiconsIcon icon={Delete02Icon} size={12} />
            </Button>
          </div>
        ))}
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit gap-1.5 text-xs"
          onClick={add}
        >
          <HugeiconsIcon icon={PlusSignIcon} size={12} />
          Add variable
        </Button>
      </div>
    </Section>
  );
}

// ── Lifecycle ───────────────────────────────────────────────────────────

function LifecycleSection({ config, onChange }: Props) {
  return (
    <Section
      title="Lifecycle hooks"
      description="post_build bakes into the image (paid once). The three pre_* hooks run every Pod start (paid every time)."
    >
      <Tabs defaultValue="post_build" className="flex w-full flex-col">
        <TabsList className="w-fit">
          <TabsTrigger value="post_build">post_build</TabsTrigger>
          <TabsTrigger value="pre_anything">pre_anything</TabsTrigger>
          <TabsTrigger value="pre_task">pre_task</TabsTrigger>
          <TabsTrigger value="pre_verification">pre_verification</TabsTrigger>
        </TabsList>
        <TabsContent value="post_build" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.post_build}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, post_build: next } })
            }
            emptyHint="Anything you want bundled into the image goes here. Runs as RUN lines at image-build time — `curl -L … | tar -x`, `pip install`, custom binary installs — paid once per config change, not per Pod start."
          />
        </TabsContent>
        <TabsContent value="pre_anything" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.pre_anything}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, pre_anything: next } })
            }
            emptyHint="Runs in every Pod djinn starts (warm AND task-run), before any djinn work. Use for per-Pod setup that doesn't belong in the image."
          />
        </TabsContent>
        <TabsContent value="pre_task" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.pre_task}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, pre_task: next } })
            }
            emptyHint="Task-run Pods only. Runs after pre_anything, before the supervisor starts."
          />
        </TabsContent>
        <TabsContent value="pre_verification" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.pre_verification}
            onChange={(next) =>
              onChange({
                ...config,
                lifecycle: { ...config.lifecycle, pre_verification: next },
              })
            }
            emptyHint="Runs once per task, before any verification rule fires. Typical use: pnpm install, cargo build, etc."
          />
        </TabsContent>
      </Tabs>
    </Section>
  );
}

// ── Verification ────────────────────────────────────────────────────────

function VerificationSection({ config, onChange }: Props) {
  const updateRules = (next: EnvironmentConfig["verification"]["rules"]) => {
    onChange({ ...config, verification: { ...config.verification, rules: next } });
  };
  const addRule = () => {
    updateRules([...config.verification.rules, { match_pattern: "", commands: [""] }]);
  };

  return (
    <Section
      title="Verification"
      description="Commands that prove a task-run succeeded. Rules match on changed files via glob; commands run in the verification Pod. Prep commands live under Lifecycle → pre_verification."
    >
      <div className="flex flex-col gap-6">
        <div>
          <div className="flex flex-col gap-2">
            {config.verification.rules.length === 0 && (
              <p className="text-xs text-muted-foreground">No verification rules configured.</p>
            )}
            {config.verification.rules.map((rule, idx) => (
              <div key={idx} className="rounded-md border bg-background/30 p-3">
                <div className="flex items-center justify-between gap-2 pb-2">
                  <div className="flex flex-1 flex-col gap-1">
                    <Label className="text-xs text-muted-foreground">Match pattern (glob)</Label>
                    <Input
                      value={rule.match_pattern}
                      onChange={(e) => {
                        const next = config.verification.rules.slice();
                        next[idx] = { ...rule, match_pattern: e.target.value };
                        updateRules(next);
                      }}
                      placeholder="src/**/*.rs"
                      className="font-mono text-xs"
                    />
                  </div>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-7 gap-1 self-end px-2 text-muted-foreground hover:text-red-400"
                    onClick={() => {
                      const next = config.verification.rules.slice();
                      next.splice(idx, 1);
                      updateRules(next);
                    }}
                  >
                    <HugeiconsIcon icon={Delete02Icon} size={14} />
                  </Button>
                </div>
                <div>
                  <Label className="text-xs text-muted-foreground">Commands (one per line)</Label>
                  <Textarea
                    value={rule.commands.join("\n")}
                    onChange={(e) => {
                      const next = config.verification.rules.slice();
                      next[idx] = {
                        ...rule,
                        commands: e.target.value
                          .split("\n")
                          .map((s) => s.trim())
                          .filter(Boolean),
                      };
                      updateRules(next);
                    }}
                    placeholder="cargo test"
                    className="mt-1 min-h-[60px] font-mono text-xs"
                  />
                </div>
              </div>
            ))}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="w-fit gap-1.5 text-xs"
              onClick={addRule}
            >
              <HugeiconsIcon icon={PlusSignIcon} size={12} />
              Add rule
            </Button>
          </div>
        </div>
      </div>
    </Section>
  );
}

// ── Shared ──────────────────────────────────────────────────────────────

function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col gap-3">
      <div>
        <h3 className="text-sm font-semibold">{title}</h3>
        {description && (
          <p className="text-xs text-muted-foreground">{description}</p>
        )}
      </div>
      {children}
    </section>
  );
}
