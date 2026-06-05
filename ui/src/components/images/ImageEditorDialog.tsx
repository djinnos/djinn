/**
 * ImageEditorDialog — create or edit a catalog image.
 *
 * Two panes mirror ProjectEnvironmentPage: a Form editor (name,
 * description, ImageConfigEditor) and a Raw JSON escape hatch for the
 * config. On save it calls `image_create` / `image_update` and surfaces
 * server-side validation errors via toast.
 */
import { useEffect, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
  listServicePresets,
  setImageAllowedPresets,
  type CatalogImage,
  type ServicePreset,
} from "@/api/images";
import { ImageConfigEditor } from "@/components/images/ImageConfigEditor";
import { showToast } from "@/lib/toast";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** When set, the dialog edits this image; otherwise it creates a new one. */
  image?: CatalogImage | null;
  onSaved: () => void | Promise<void>;
}

function emptyConfig(): EnvironmentConfig {
  return normalizeConfig({ schema_version: 1, source: "user-edited" });
}

export function ImageEditorDialog({ open, onOpenChange, image, onSaved }: Props) {
  const isEdit = Boolean(image);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [config, setConfig] = useState<EnvironmentConfig>(emptyConfig);
  const [rawText, setRawText] = useState("");
  const [rawError, setRawError] = useState<string | null>(null);
  const [mode, setMode] = useState<"form" | "raw">("form");
  const [saving, setSaving] = useState(false);
  const [presets, setPresets] = useState<ServicePreset[]>([]);
  const [selectedPresets, setSelectedPresets] = useState<string[]>([]);

  // Load the fixed service-preset catalog once the dialog is opened.
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    void listServicePresets()
      .then((list) => {
        if (!cancelled) setPresets(list);
      })
      .catch((err) => {
        const message =
          err instanceof Error ? err.message : "Failed to load service presets";
        showToast.error("Could not load service presets", { description: message });
      });
    return () => {
      cancelled = true;
    };
  }, [open]);

  // Reseed local state whenever the dialog opens (or the target image
  // changes) so create and edit don't leak each other's fields.
  useEffect(() => {
    if (!open) return;
    const seed = image ? image.config : emptyConfig();
    setName(image?.name ?? "");
    setDescription(image?.description ?? "");
    setConfig(seed);
    setRawText(JSON.stringify(seed, null, 2));
    setRawError(null);
    setMode("form");
    setSelectedPresets(image?.allowedPresets ?? []);
  }, [open, image]);

  const togglePreset = (id: string, checked: boolean) => {
    setSelectedPresets((prev) =>
      checked ? [...new Set([...prev, id])] : prev.filter((p) => p !== id),
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
      toSave = parsed.value as EnvironmentConfig;
    }
    setSaving(true);
    try {
      const result = isEdit && image
        ? await updateImage({
            id: image.id,
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
      const imageId = isEdit && image ? image.id : result.id;
      if (imageId) {
        const presetResult = await setImageAllowedPresets(imageId, selectedPresets);
        if (!presetResult.ok) {
          showToast.error("Saved image, but allowed services failed to update", {
            description: presetResult.error,
          });
        }
      }
      showToast.success(isEdit ? "Image updated" : "Image created");
      await onSaved();
      onOpenChange(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : "Save failed";
      showToast.error("Save failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>{isEdit ? "Edit image" : "New image"}</DialogTitle>
          <DialogDescription>
            A reusable environment preset (languages + versions, system packages, build env)
            that any project can adopt.
          </DialogDescription>
        </DialogHeader>

        <div className="flex max-h-[60vh] flex-col gap-4 overflow-y-auto pr-1">
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            <div className="flex flex-col gap-1">
              <Label className="text-xs text-muted-foreground">Name</Label>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Rust"
              />
            </div>
            <div className="flex flex-col gap-1">
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
                  className="min-h-[320px] font-mono text-xs"
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

          <div className="flex flex-col gap-2">
            <div className="flex flex-col gap-0.5">
              <Label className="text-sm font-medium">Allowed services</Label>
              <p className="text-xs text-muted-foreground">
                Projects using this image may request these services on demand during tests.
              </p>
            </div>
            {presets.length === 0 ? (
              <p className="text-xs text-muted-foreground">No service presets available.</p>
            ) : (
              <div className="flex flex-col gap-2">
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

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={saving}>
            Cancel
          </Button>
          <Button
            onClick={() => void handleSave()}
            disabled={saving || (mode === "raw" && rawError !== null)}
          >
            {saving && (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
            )}
            {saving ? "Saving…" : isEdit ? "Save changes" : "Create image"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
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
