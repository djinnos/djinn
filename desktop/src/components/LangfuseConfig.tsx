import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useSettingsStore } from "@/stores/settingsStore";
import type { LangfuseSettings } from "@/api/settings";

function LangfuseForm({ initial }: { initial: LangfuseSettings }) {
  const isSaving = useSettingsStore((s) => s.isSavingLangfuse);
  const updateLangfuse = useSettingsStore((s) => s.updateLangfuse);
  const saveLangfuse = useSettingsStore((s) => s.saveLangfuse);

  const [publicKey, setPublicKey] = useState(initial.publicKey);
  const [secretKey, setSecretKey] = useState(initial.secretKey);
  const [endpoint, setEndpoint] = useState(initial.endpoint);

  const hasChanges =
    publicKey !== initial.publicKey ||
    secretKey !== initial.secretKey ||
    endpoint !== initial.endpoint;

  const handleSave = async () => {
    const values = { publicKey, secretKey, endpoint };
    updateLangfuse(values);
    await saveLangfuse();
  };

  return (
    <div className="space-y-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-xl font-bold">Langfuse</h3>
          <p className="text-sm text-muted-foreground">
            LLM observability and tracing via OTLP export
          </p>
        </div>
        {hasChanges && (
          <Button variant="outline" size="sm" onClick={() => void handleSave()} disabled={isSaving}>
            {isSaving ? "Saving..." : "Save"}
          </Button>
        )}
      </div>

      <div className="space-y-3">
        <div className="space-y-1.5">
          <Label htmlFor="langfuse-public-key">Public Key</Label>
          <Input
            id="langfuse-public-key"
            type="text"
            placeholder="pk-lf-..."
            value={publicKey}
            onChange={(e) => setPublicKey(e.target.value)}
          />
        </div>

        <div className="space-y-1.5">
          <Label htmlFor="langfuse-secret-key">Secret Key</Label>
          <Input
            id="langfuse-secret-key"
            type="password"
            placeholder="sk-lf-..."
            value={secretKey}
            onChange={(e) => setSecretKey(e.target.value)}
          />
        </div>

        <div className="space-y-1.5">
          <Label htmlFor="langfuse-endpoint">Endpoint</Label>
          <Input
            id="langfuse-endpoint"
            type="url"
            placeholder="https://cloud.langfuse.com/api/public/otel"
            value={endpoint}
            onChange={(e) => setEndpoint(e.target.value)}
          />
          <p className="text-xs text-muted-foreground">
            Leave empty to use the default endpoint
          </p>
        </div>
      </div>
    </div>
  );
}

export function LangfuseConfig() {
  const langfuse = useSettingsStore((s) => s.langfuse);
  const isLoading = useSettingsStore((s) => s.isLoading);

  if (isLoading) return null;

  // Key resets local form state when store values change after save/load
  const key = `${langfuse.publicKey}|${langfuse.secretKey}|${langfuse.endpoint}`;
  return <LangfuseForm key={key} initial={langfuse} />;
}
