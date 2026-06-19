/**
 * ImageEditorPage — full-page create/edit flow for a catalog image.
 *
 * Routes: `/images/new` and `/images/:id/edit`. Replaces the old cramped
 * ImageEditorDialog with a full-width layout: name/description, a Form /
 * Raw JSON tab pair (reusing ImageConfigEditor), and the injected-services
 * picker. On save it calls `image_create` / `image_update` +
 * `image_set_services`, then navigates back to `/images`.
 */
import { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  ArrowLeft02Icon,
  FloppyDiskIcon,
  Loading02Icon,
} from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  type EnvironmentConfig,
  normalizeConfig,
} from "@/api/environmentConfig";
import {
  createImage,
  updateImage,
  listImages,
  listServicePresets,
  setImageServices,
  type ServicePreset,
} from "@/api/images";
import { ImageConfigEditor } from "@/components/images/ImageConfigEditor";
import { showToast } from "@/lib/toast";

function emptyConfig(): EnvironmentConfig {
  return normalizeConfig({ schema_version: 1, source: "user-edited" });
}

export function ImageEditorPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const isEdit = Boolean(id);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [config, setConfig] = useState<EnvironmentConfig>(emptyConfig);
  const [rawText, setRawText] = useState(() => JSON.stringify(emptyConfig(), null, 2));
  const [rawError, setRawError] = useState<string | null>(null);
  const [mode, setMode] = useState<"form" | "raw">("form");
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(isEdit);
  const [presets, setPresets] = useState<ServicePreset[]>([]);
  const [selectedPresets, setSelectedPresets] = useState<string[]>([]);

  // Load the fixed service-preset catalog + (when editing) the target image.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [presetList, images] = await Promise.all([
          listServicePresets(),
          isEdit ? listImages() : Promise.resolve([]),
        ]);
        if (cancelled) return;
        setPresets(presetList);
        if (isEdit) {
          const image = images.find((img) => img.id === id);
          if (!image) {
            showToast.error("Image not found");
            navigate("/images");
            return;
          }
          const seed = image.config;
          setName(image.name);
          setDescription(image.description ?? "");
          setConfig(seed);
          setRawText(JSON.stringify(seed, null, 2));
          setSelectedPresets(image.servicePresets);
        }
      } catch (err) {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : "Failed to load";
        showToast.error("Could not load image editor", { description: message });
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id, isEdit, navigate]);

  const togglePreset = (presetId: string, checked: boolean) => {
    setSelectedPresets((prev) =>
      checked ? [...new Set([...prev, presetId])] : prev.filter((p) => p !== presetId),
    );
  };

  const handleModeChange = (next: string) => {
    if (next === mode) return;
    if (next === "raw") {
      setRawText(JSON.stringify(config, null, 2));
      setRawError(null);
    }
    if (next === "form") {
      const parsed = tryParse(rawText);
      if (!parsed.ok) {
        setRawError(parsed.error);
        return;
      }
      setConfig(normalizeConfig(parsed.value));
      setRawError(null);
    }
    setMode(next as "form" | "raw");
  };

  const handleFormChange = (next: EnvironmentConfig) => {
    setConfig(next);
    setRawText(JSON.stringify(next, null, 2));
  };

  const handleRawChange = (next: string) => {
    setRawText(next);
    const parsed = tryParse(next);
    if (parsed.ok) {
      setRawError(null);
      setConfig(normalizeConfig(parsed.value));
    } else {
      setRawError(parsed.error);
    }
  };

  const handleSave = async () => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      showToast.error("Name is required");
      return;
    }
    let toSave: EnvironmentConfig = config;
    if (mode === "raw") {
      const parsed = tryParse(rawText);
      if (!parsed.ok) {
        showToast.error("Cannot save — JSON is invalid", { description: parsed.error });
        return;
      }
      toSave = normalizeConfig(parsed.value);
    }
    setSaving(true);
    try {
      const result =
        isEdit && id
          ? await updateImage({
              id,
              name: trimmedName,
              description: description.trim() || undefined,
              config: toSave,
            })
          : await createImage({
              name: trimmedName,
              description: description.trim() || undefined,
              config: toSave,
            });
      if (!result.ok) {
        showToast.error(isEdit ? "Update failed" : "Create failed", {
          description: result.error,
        });
        return;
      }
      const imageId = isEdit && id ? id : result.id;
      if (imageId) {
        const presetResult = await setImageServices(imageId, selectedPresets);
        if (!presetResult.ok) {
          showToast.error("Saved image, but injected services failed to update", {
            description: presetResult.error,
          });
        }
      }
      showToast.success(isEdit ? "Image updated" : "Image created");
      navigate("/images");
    } catch (err) {
      const message = err instanceof Error ? err.message : "Save failed";
      showToast.error("Save failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <HugeiconsIcon
          icon={Loading02Icon}
          className="size-5 animate-spin text-muted-foreground"
        />
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between gap-3 border-b px-6 py-4">
        <div className="flex items-center gap-3">
          <Button
            variant="ghost"
            size="sm"
            className="h-8 gap-1.5 px-2 text-xs"
            onClick={() => navigate("/images")}
          >
            <HugeiconsIcon icon={ArrowLeft02Icon} size={14} />
            Back
          </Button>
          <div>
            <h1 className="text-lg font-semibold">{isEdit ? "Edit image" : "New image"}</h1>
            <p className="text-xs text-muted-foreground">
              A reusable environment preset (languages + versions, system packages, build env)
              that any project can adopt.
            </p>
          </div>
        </div>
        <Button
          size="sm"
          className="h-8 gap-1.5 text-xs"
          onClick={() => void handleSave()}
          disabled={saving || (mode === "raw" && rawError !== null)}
        >
          {saving ? (
            <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
          ) : (
            <HugeiconsIcon icon={FloppyDiskIcon} size={14} />
          )}
          {saving ? "Saving…" : isEdit ? "Save changes" : "Create image"}
        </Button>
      </header>

      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto flex max-w-5xl flex-col gap-6">
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="flex flex-col gap-1.5">
              <Label className="text-xs text-muted-foreground">Name</Label>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Rust"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label className="text-xs text-muted-foreground">Description</Label>
              <Input
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                placeholder="optional"
              />
            </div>
          </div>

          <Tabs
            value={mode}
            onValueChange={(v) => typeof v === "string" && handleModeChange(v)}
            className="flex flex-col"
          >
            <TabsList className="w-fit">
              <TabsTrigger value="form">Form</TabsTrigger>
              <TabsTrigger value="raw">Raw JSON</TabsTrigger>
            </TabsList>
            <TabsContent value="form" className="mt-4">
              <ImageConfigEditor config={config} onChange={handleFormChange} />
            </TabsContent>
            <TabsContent value="raw" className="mt-4">
              <div className="flex flex-col gap-2">
                <Textarea
                  value={rawText}
                  onChange={(e) => handleRawChange(e.target.value)}
                  className="min-h-[480px] font-mono text-xs"
                  spellCheck={false}
                />
                {rawError ? (
                  <p className="text-xs text-destructive">{rawError}</p>
                ) : (
                  <p className="text-xs text-muted-foreground">
                    JSON is valid. Server-side validation still runs on save.
                  </p>
                )}
              </div>
            </TabsContent>
          </Tabs>

          <div className="flex flex-col gap-3">
            <div className="flex flex-col gap-0.5">
              <Label className="text-sm font-medium">Injected services</Label>
              <p className="text-xs text-muted-foreground">
                These services are injected as sidecars into every task-run
                Pod that uses this image — reachable on 127.0.0.1 with the connection string in
                the preset's env var (e.g. TEST_POSTGRES_URL).
              </p>
            </div>
            {presets.length === 0 ? (
              <p className="text-xs text-muted-foreground">No service presets available.</p>
            ) : (
              <div className="grid grid-cols-1 gap-2 md:grid-cols-2">
                {presets.map((preset) => (
                  <div
                    key={preset.id}
                    className="flex items-center justify-between gap-2 rounded-md border bg-background/30 px-3 py-2.5"
                  >
                    <div className="min-w-0">
                      <div className="text-sm font-medium">{preset.name}</div>
                      <div className="truncate text-xs text-muted-foreground">
                        {preset.serviceType}
                      </div>
                    </div>
                    <Switch
                      checked={selectedPresets.includes(preset.id)}
                      onCheckedChange={(v) => togglePreset(preset.id, v)}
                    />
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────

type ParseResult<T> = { ok: true; value: T } | { ok: false; error: string };

function tryParse(text: string): ParseResult<unknown> {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : "invalid JSON" };
  }
}
