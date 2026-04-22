/**
 * EnvironmentConfigForm — the form-based editor for a single project's
 * `environment_config`. Panes: Base image, Languages, Workspaces,
 * System packages, Env vars, Lifecycle hooks, Verification.
 *
 * The form binds against the normalized `EnvironmentConfig` shape from
 * `@/api/environmentConfig`; any field unknown to the form passes through
 * untouched so manual JSON edits round-trip cleanly.
 */
import { useCallback } from "react";
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
      <BaseImageSection config={config} onChange={onChange} />
      <LanguagesSection config={config} onChange={onChange} />
      <WorkspacesSection config={config} onChange={onChange} />
      <SystemPackagesSection config={config} onChange={onChange} />
      <EnvVarsSection config={config} onChange={onChange} />
      <LifecycleSection config={config} onChange={onChange} />
      <VerificationSection config={config} onChange={onChange} />
    </div>
  );
}

// ── Base image ──────────────────────────────────────────────────────────

function BaseImageSection({ config, onChange }: Props) {
  return (
    <Section
      title="Base image"
      description="Starting point for the generated Dockerfile. Variant must match an available Debian/Alpine tag on Docker Hub."
    >
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <div className="flex flex-col gap-1.5">
          <Label>Distro</Label>
          <Select
            value={config.base.distro}
            onValueChange={(v) => {
              if (v === "debian" || v === "alpine") {
                onChange({ ...config, base: { ...config.base, distro: v } });
              }
            }}
          >
            <SelectTrigger className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="debian">debian</SelectItem>
              <SelectItem value="alpine">alpine</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className="flex flex-col gap-1.5">
          <Label>Variant</Label>
          <Input
            value={config.base.variant}
            onChange={(e) =>
              onChange({ ...config, base: { ...config.base, variant: e.target.value } })
            }
            placeholder="bookworm-slim"
            className="font-mono text-xs"
          />
        </div>
      </div>
    </Section>
  );
}

// ── Languages ───────────────────────────────────────────────────────────

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

function LanguagesSection({ config, onChange }: Props) {
  const set = (langs: Languages) => onChange({ ...config, languages: langs });

  return (
    <Section
      title="Languages"
      description="Enable a language to install its toolchain in the generated image. Per-workspace overrides below."
    >
      <div className="flex flex-col gap-3">
        {LANGUAGE_KEYS.map((lang) => (
          <LanguageBlock
            key={lang}
            lang={lang}
            languages={config.languages}
            onChange={set}
          />
        ))}
      </div>
    </Section>
  );
}

function LanguageBlock({
  lang,
  languages,
  onChange,
}: {
  lang: LanguageKey;
  languages: Languages;
  onChange: (next: Languages) => void;
}) {
  const enabled = languages[lang] !== undefined;

  const toggle = () => {
    if (enabled) {
      const next = { ...languages };
      delete next[lang];
      onChange(next);
    } else {
      onChange({ ...languages, [lang]: defaultLanguageBlock(lang) });
    }
  };

  return (
    <div className="rounded-md border bg-background/30">
      <div className="flex items-center justify-between gap-2 border-b border-border/40 px-3 py-2">
        <span className="text-sm font-medium">{LANGUAGE_LABELS[lang]}</span>
        <button
          type="button"
          role="switch"
          aria-checked={enabled}
          onClick={toggle}
          className={
            "relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors " +
            (enabled ? "bg-primary" : "bg-muted")
          }
        >
          <span
            className={
              "inline-block h-4 w-4 transform rounded-full bg-background shadow transition-transform " +
              (enabled ? "translate-x-4" : "translate-x-0.5")
            }
          />
        </button>
      </div>
      {enabled && (
        <div className="px-3 py-3">
          <LanguageFields lang={lang} languages={languages} onChange={onChange} />
        </div>
      )}
    </div>
  );
}

function defaultLanguageBlock(lang: LanguageKey): NonNullable<Languages[LanguageKey]> {
  switch (lang) {
    case "rust":
      return { default_toolchain: "stable", components: ["rust-analyzer"], targets: [] };
    case "node":
      return { default_version: "22", default_package_manager: "pnpm", scip_indexer: "scip-typescript" };
    case "python":
      return { default_version: "3.12", scip_indexer: "scip-python" };
    case "go":
      return { default_version: "1.22", scip_indexer: "scip-go" };
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

function LanguageFields({
  lang,
  languages,
  onChange,
}: {
  lang: LanguageKey;
  languages: Languages;
  onChange: (next: Languages) => void;
}) {
  // Narrowed via `unknown` cast — each language has a slightly different
  // shape but the UI only reads the fields the shape actually has.
  const block = languages[lang] as unknown as Record<string, unknown>;
  const update = (patch: Record<string, unknown>) => {
    onChange({ ...languages, [lang]: { ...block, ...patch } });
  };

  if (lang === "rust") {
    const components = (block.components as string[] | undefined) ?? [];
    const targets = (block.targets as string[] | undefined) ?? [];
    return (
      <div className="flex flex-col gap-2.5">
        <InlineField
          label="Default toolchain"
          value={(block.default_toolchain as string) ?? ""}
          onChange={(v) => update({ default_toolchain: v })}
          placeholder="stable"
        />
        <StringListField
          label="Components"
          values={components}
          onChange={(v) => update({ components: v })}
          placeholder="rust-analyzer"
        />
        <StringListField
          label="Targets"
          values={targets}
          onChange={(v) => update({ targets: v })}
          placeholder="wasm32-unknown-unknown"
        />
      </div>
    );
  }

  if (lang === "node") {
    return (
      <div className="grid grid-cols-1 gap-2.5 md:grid-cols-3">
        <InlineField
          label="Default version"
          value={(block.default_version as string) ?? ""}
          onChange={(v) => update({ default_version: v })}
          placeholder="22"
        />
        <InlineField
          label="Package manager"
          value={(block.default_package_manager as string) ?? ""}
          onChange={(v) => update({ default_package_manager: v || null })}
          placeholder="pnpm"
        />
        <InlineField
          label="SCIP indexer"
          value={(block.scip_indexer as string) ?? ""}
          onChange={(v) => update({ scip_indexer: v || null })}
          placeholder="scip-typescript"
        />
      </div>
    );
  }

  // Generic shape: default_version + scip_indexer
  return (
    <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2">
      <InlineField
        label="Default version"
        value={(block.default_version as string) ?? ""}
        onChange={(v) => update({ default_version: v })}
        placeholder="latest"
      />
      <InlineField
        label="SCIP indexer"
        value={(block.scip_indexer as string) ?? ""}
        onChange={(v) => update({ scip_indexer: v || null })}
        placeholder="(optional)"
      />
    </div>
  );
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

function StringListField({
  label,
  values,
  onChange,
  placeholder,
}: {
  label: string;
  values: string[];
  onChange: (v: string[]) => void;
  placeholder?: string;
}) {
  const update = (idx: number, v: string) => {
    const next = values.slice();
    next[idx] = v;
    onChange(next);
  };
  const remove = (idx: number) => {
    const next = values.slice();
    next.splice(idx, 1);
    onChange(next);
  };
  return (
    <div className="flex flex-col gap-1">
      <Label className="text-xs text-muted-foreground">{label}</Label>
      <div className="flex flex-col gap-1.5">
        {values.map((v, idx) => (
          <div key={idx} className="flex items-center gap-1.5">
            <Input
              value={v}
              onChange={(e) => update(idx, e.target.value)}
              placeholder={placeholder}
              className="font-mono text-xs"
            />
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
              onClick={() => remove(idx)}
            >
              <HugeiconsIcon icon={Delete02Icon} size={12} />
            </Button>
          </div>
        ))}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-7 w-fit gap-1 px-2 text-xs text-muted-foreground"
          onClick={() => onChange([...values, ""])}
        >
          <HugeiconsIcon icon={PlusSignIcon} size={12} />
          Add
        </Button>
      </div>
    </div>
  );
}

// ── Workspaces ──────────────────────────────────────────────────────────

function WorkspacesSection({ config, onChange }: Props) {
  const update = useCallback(
    (next: Workspace[]) => onChange({ ...config, workspaces: next }),
    [config, onChange],
  );

  const updateAt = (idx: number, patch: Partial<Workspace>) => {
    const next = config.workspaces.slice();
    next[idx] = { ...next[idx], ...patch };
    update(next);
  };

  const remove = (idx: number) => {
    const next = config.workspaces.slice();
    next.splice(idx, 1);
    update(next);
  };

  const add = () => {
    update([
      ...config.workspaces,
      { slug: "", root: "", language: "rust" },
    ]);
  };

  return (
    <Section
      title="Workspaces"
      description="Per-workspace toolchain overrides. Slug is unique per project; root is a repo-relative path. Rust workspaces use `toolchain`, others use `version`."
    >
      <div className="overflow-x-auto rounded-md border">
        <table className="w-full text-left text-xs">
          <thead className="bg-white/[0.02] text-[11px] uppercase tracking-wide text-muted-foreground">
            <tr>
              <th className="px-2 py-2 font-medium">Slug</th>
              <th className="px-2 py-2 font-medium">Root</th>
              <th className="px-2 py-2 font-medium">Language</th>
              <th className="px-2 py-2 font-medium">Toolchain</th>
              <th className="px-2 py-2 font-medium">Version</th>
              <th className="px-2 py-2 font-medium">Package mgr.</th>
              <th className="px-2 py-2"></th>
            </tr>
          </thead>
          <tbody>
            {config.workspaces.length === 0 && (
              <tr>
                <td colSpan={7} className="px-2 py-3 text-muted-foreground">
                  No workspaces configured. Detection populates these on first boot.
                </td>
              </tr>
            )}
            {config.workspaces.map((ws, idx) => (
              <tr key={idx} className="border-t border-border/40">
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.slug}
                    onChange={(e) => updateAt(idx, { slug: e.target.value })}
                    className="h-7 font-mono text-xs"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.root}
                    onChange={(e) => updateAt(idx, { root: e.target.value })}
                    className="h-7 font-mono text-xs"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.language}
                    onChange={(e) => updateAt(idx, { language: e.target.value })}
                    className="h-7 w-24 font-mono text-xs"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.toolchain ?? ""}
                    onChange={(e) => updateAt(idx, { toolchain: e.target.value || null })}
                    className="h-7 font-mono text-xs"
                    placeholder="—"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.version ?? ""}
                    onChange={(e) => updateAt(idx, { version: e.target.value || null })}
                    className="h-7 font-mono text-xs"
                    placeholder="—"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Input
                    value={ws.package_manager ?? ""}
                    onChange={(e) => updateAt(idx, { package_manager: e.target.value || null })}
                    className="h-7 font-mono text-xs"
                    placeholder="—"
                  />
                </td>
                <td className="px-2 py-1.5">
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-7 w-7 p-0 text-muted-foreground hover:text-red-400"
                    onClick={() => remove(idx)}
                  >
                    <HugeiconsIcon icon={Delete02Icon} size={12} />
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="mt-3 w-fit gap-1.5 text-xs"
        onClick={add}
      >
        <HugeiconsIcon icon={PlusSignIcon} size={12} />
        Add workspace
      </Button>
    </Section>
  );
}

// ── System packages ─────────────────────────────────────────────────────

function SystemPackagesSection({ config, onChange }: Props) {
  return (
    <Section
      title="System packages"
      description="One package name per line. apt is used on debian, apk on alpine."
    >
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <div className="flex flex-col gap-1">
          <Label>apt (debian)</Label>
          <Textarea
            value={config.system_packages.apt.join("\n")}
            onChange={(e) =>
              onChange({
                ...config,
                system_packages: {
                  ...config.system_packages,
                  apt: e.target.value
                    .split("\n")
                    .map((s) => s.trim())
                    .filter(Boolean),
                },
              })
            }
            placeholder="postgresql-client"
            className="min-h-[100px] font-mono text-xs"
          />
        </div>
        <div className="flex flex-col gap-1">
          <Label>apk (alpine)</Label>
          <Textarea
            value={config.system_packages.apk.join("\n")}
            onChange={(e) =>
              onChange({
                ...config,
                system_packages: {
                  ...config.system_packages,
                  apk: e.target.value
                    .split("\n")
                    .map((s) => s.trim())
                    .filter(Boolean),
                },
              })
            }
            placeholder="postgresql-client"
            className="min-h-[100px] font-mono text-xs"
          />
        </div>
      </div>
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
      description="post_build runs as RUN lines in the generated image; pre_warm runs in the warm Pod; pre_task runs in each task-run Pod."
    >
      <Tabs defaultValue="post_build" className="flex w-full flex-col">
        <TabsList className="w-fit">
          <TabsTrigger value="post_build">post_build</TabsTrigger>
          <TabsTrigger value="pre_warm">pre_warm</TabsTrigger>
          <TabsTrigger value="pre_task">pre_task</TabsTrigger>
        </TabsList>
        <TabsContent value="post_build" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.post_build}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, post_build: next } })
            }
            emptyHint="No post-build hooks. Commands here are baked into the image as RUN lines."
          />
        </TabsContent>
        <TabsContent value="pre_warm" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.pre_warm}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, pre_warm: next } })
            }
            emptyHint="No pre-warm hooks. Commands here run before the canonical graph indexers."
          />
        </TabsContent>
        <TabsContent value="pre_task" className="mt-3">
          <HookCommandList
            hooks={config.lifecycle.pre_task}
            onChange={(next) =>
              onChange({ ...config, lifecycle: { ...config.lifecycle, pre_task: next } })
            }
            emptyHint="No pre-task hooks. Commands here run before the supervisor starts in each task Pod."
          />
        </TabsContent>
      </Tabs>
    </Section>
  );
}

// ── Verification ────────────────────────────────────────────────────────

function VerificationSection({ config, onChange }: Props) {
  const updateSetup = (next: EnvironmentConfig["verification"]["setup"]) => {
    onChange({ ...config, verification: { ...config.verification, setup: next } });
  };
  const updateRules = (next: EnvironmentConfig["verification"]["rules"]) => {
    onChange({ ...config, verification: { ...config.verification, rules: next } });
  };
  const addRule = () => {
    updateRules([...config.verification.rules, { match_pattern: "", commands: [""] }]);
  };

  return (
    <Section
      title="Verification"
      description="Commands that prove a task-run succeeded. Rules match on changed files via glob; commands run in the verification Pod."
    >
      <div className="flex flex-col gap-6">
        <div>
          <h4 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Setup
          </h4>
          <HookCommandList
            hooks={config.verification.setup}
            onChange={updateSetup}
            emptyHint="No setup hooks. These run before the match-based rule commands."
          />
        </div>

        <div>
          <h4 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Rules
          </h4>
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
