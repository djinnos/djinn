/**
 * ImageConfigEditor — the form editor for a catalog image's
 * `EnvironmentConfig` build fields (languages + versions, system packages,
 * build env). Mirrors the per-project `EnvironmentConfigForm` patterns but
 * is centered on language toolchains since catalog images are language
 * presets.
 *
 * Each language has a curated version preset list (a Select) plus a
 * "custom" escape hatch (a free-text Input). Multi-language images are
 * allowed: enable as many languages as you want.
 */
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Delete02Icon, PlusSignIcon } from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  type EnvironmentConfig,
  type LanguageKey,
  type Languages,
} from "@/api/environmentConfig";
import { listToolchainVersions } from "@/api/images";

interface Props {
  config: EnvironmentConfig;
  onChange: (next: EnvironmentConfig) => void;
}

// ── Language metadata ─────────────────────────────────────────────────────

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

// Languages we expose a curated picker for. (The other LANGUAGE_KEYS still
// round-trip via the Raw JSON tab.)
const EDITABLE_LANGUAGES: LanguageKey[] = ["rust", "node", "python", "go"];

const CUSTOM = "__custom__";

interface LangSpec {
  /** The `languages.X` field holding the version string. */
  field: "default_toolchain" | "default_version";
  presets: string[];
  default: string;
}

const LANG_SPECS: Record<"rust" | "node" | "python" | "go", LangSpec> = {
  rust: { field: "default_toolchain", presets: ["stable", "beta", "nightly"], default: "stable" },
  node: { field: "default_version", presets: ["lts", "22", "20"], default: "lts" },
  python: { field: "default_version", presets: ["3.13", "3.12"], default: "3.13" },
  go: { field: "default_version", presets: ["1.23", "1.22"], default: "1.23" },
};

/**
 * Shared, one-shot fetch of the live per-language version lists. Falls back
 * to the static `LANG_SPECS` presets while loading or on error.
 */
function useToolchainVersions() {
  return useQuery({
    queryKey: ["toolchain-versions"],
    queryFn: listToolchainVersions,
    staleTime: 5 * 60 * 1000,
    retry: 1,
  });
}

/**
 * Build a `languages.X` block. The block shape differs per language
 * (Rust uses `default_toolchain`, the rest `default_version`); the union
 * type can't be indexed by a dynamic key, so we build the single-field
 * object and assert it into the slot's value type.
 */
function makeBlock(
  field: "default_toolchain" | "default_version",
  value: string,
): Record<string, string> {
  return { [field]: value };
}

function readVersion(config: EnvironmentConfig, lang: keyof typeof LANG_SPECS): string {
  const block = config.languages[lang] as Record<string, string> | undefined;
  if (!block) return LANG_SPECS[lang].default;
  return block[LANG_SPECS[lang].field] ?? LANG_SPECS[lang].default;
}

export function ImageConfigEditor({ config, onChange }: Props) {
  return (
    <div className="flex flex-col gap-6">
      <LanguagesSection config={config} onChange={onChange} />
      <SystemPackagesSection config={config} onChange={onChange} />
      <EnvVarsSection config={config} onChange={onChange} />
    </div>
  );
}

// ── Languages ─────────────────────────────────────────────────────────────

function LanguagesSection({ config, onChange }: Props) {
  const versionsQuery = useToolchainVersions();

  const presetsFor = (lang: keyof typeof LANG_SPECS): string[] => {
    const live = versionsQuery.data?.[lang];
    return live && live.length > 0 ? live : LANG_SPECS[lang].presets;
  };

  const setLanguage = (lang: keyof typeof LANG_SPECS, enabled: boolean) => {
    // A union-keyed write demands the intersection of all value types, so go
    // through a Record view to assign the single-language block.
    const nextLanguages = { ...config.languages } as Record<string, unknown>;
    if (enabled) {
      const spec = LANG_SPECS[lang];
      nextLanguages[lang] = makeBlock(spec.field, spec.default);
    } else {
      delete nextLanguages[lang];
    }
    onChange({ ...config, languages: nextLanguages as Languages });
  };

  const setVersion = (lang: keyof typeof LANG_SPECS, version: string) => {
    const spec = LANG_SPECS[lang];
    const nextLanguages = { ...config.languages } as Record<string, unknown>;
    nextLanguages[lang] = makeBlock(spec.field, version);
    onChange({ ...config, languages: nextLanguages as Languages });
  };

  return (
    <Section
      title="Languages"
      description="Pick the toolchains this image installs. Enable as many as you need — the image installs all of them."
    >
      <div className="flex flex-col gap-3">
        {EDITABLE_LANGUAGES.map((lang) => {
          const spec = LANG_SPECS[lang as keyof typeof LANG_SPECS];
          const enabled = config.languages[lang] !== undefined;
          return (
            <div key={lang} className="rounded-md border bg-background/30">
              <div className="flex items-center justify-between gap-2 px-3 py-2.5">
                <Label className="text-sm font-medium">{LANGUAGE_LABELS[lang]}</Label>
                <Switch
                  checked={enabled}
                  onCheckedChange={(v) => setLanguage(lang as keyof typeof LANG_SPECS, v)}
                />
              </div>
              {enabled && (
                <div className="border-t border-border/40 px-3 py-3">
                  <VersionSelector
                    label={spec.field === "default_toolchain" ? "Toolchain" : "Version"}
                    presets={presetsFor(lang as keyof typeof LANG_SPECS)}
                    value={readVersion(config, lang as keyof typeof LANG_SPECS)}
                    onChange={(v) => setVersion(lang as keyof typeof LANG_SPECS, v)}
                  />
                </div>
              )}
            </div>
          );
        })}
      </div>
    </Section>
  );
}

function VersionSelector({
  label,
  presets,
  value,
  onChange,
}: {
  label: string;
  presets: string[];
  value: string;
  onChange: (v: string) => void;
}) {
  // If the stored value isn't in the fetched/static preset list, treat it
  // as custom: show it as the selected value AND reveal the text input.
  const valueIsPreset = presets.includes(value);
  // Explicit custom toggle, so picking "Custom…" reveals the input even
  // when the current value happens to coincide with a preset.
  const [customMode, setCustomMode] = useState(!valueIsPreset);
  const showCustom = customMode || !valueIsPreset;
  const selectValue = showCustom ? CUSTOM : value;

  return (
    <div className="flex flex-col gap-2">
      <Label className="text-xs text-muted-foreground">{label}</Label>
      <div className="flex items-center gap-2">
        <Select
          value={selectValue}
          onValueChange={(v) => {
            if (typeof v !== "string") return;
            if (v === CUSTOM) {
              setCustomMode(true);
              return;
            }
            setCustomMode(false);
            onChange(v);
          }}
        >
          <SelectTrigger className="w-[160px]">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {presets.map((preset) => (
              <SelectItem key={preset} value={preset}>
                {preset}
              </SelectItem>
            ))}
            <SelectItem value={CUSTOM}>Custom…</SelectItem>
          </SelectContent>
        </Select>
        {showCustom && (
          <Input
            value={value}
            onChange={(e) => onChange(e.target.value)}
            placeholder="custom version"
            className="flex-1 font-mono text-xs"
            autoFocus
          />
        )}
      </div>
    </div>
  );
}

// ── System packages ───────────────────────────────────────────────────────

function SystemPackagesSection({ config, onChange }: Props) {
  return (
    <Section
      title="System packages"
      description="One apt package name per line. For anything not in apt, use a post_build hook (Raw JSON tab)."
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
        className="min-h-[80px] font-mono text-xs"
      />
    </Section>
  );
}

// ── Env vars ──────────────────────────────────────────────────────────────

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
      description="Baked into the image as ENV lines. Keys must match [A-Za-z_][A-Za-z0-9_]*."
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

// ── Shared ────────────────────────────────────────────────────────────────

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
        {description && <p className="text-xs text-muted-foreground">{description}</p>}
      </div>
      {children}
    </section>
  );
}
