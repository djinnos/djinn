import { useEffect, useState } from "react";
import {
  fetchProviderCatalog,
  validateProviderApiKey,
  saveProviderCredentials,
  startProviderOAuth,
  type Provider,
} from "@/api/server";
import { useWizardStore } from "@/stores/wizardStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Field,
  FieldLabel,
  FieldDescription,
  FieldError,
} from "@/components/ui/field";
import { Loader2Icon, CheckCircle2Icon, AlertCircleIcon } from "lucide-react";

export function ProviderSetupStep() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [selectedProvider, setSelectedProvider] = useState<string>("");
  const [apiKey, setApiKey] = useState("");
  const [isLoading, setIsLoading] = useState(true);
  const [isValidating, setIsValidating] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [isOAuthInProgress, setIsOAuthInProgress] = useState(false);
  const [isValidated, setIsValidated] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const { nextStep } = useWizardStore();

  // Load provider catalog on mount
  useEffect(() => {
    loadProviders();
  }, []);

  const loadProviders = async () => {
    setIsLoading(true);
    setLoadError(null);
    try {
      const catalog = await fetchProviderCatalog();
      setProviders(catalog);
    } catch (error) {
      setLoadError(
        error instanceof Error
          ? error.message
          : "Failed to load provider catalog"
      );
    } finally {
      setIsLoading(false);
    }
  };

  // Handle provider selection
  const handleProviderChange = (value: string | null) => {
    if (value) {
      setSelectedProvider(value);
      setApiKey("");
      setIsValidated(false);
      setValidationError(null);
    }
  };

  // Validate API key inline
  const validateKey = async () => {
    if (!selectedProvider || !apiKey.trim()) return false;

    setIsValidating(true);
    setIsValidated(false);
    setValidationError(null);

    try {
      const result = await validateProviderApiKey(selectedProvider, apiKey);
      if (result.valid) {
        setIsValidated(true);
        return true;
      } else {
        setIsValidated(false);
        setValidationError(result.error || "Invalid API key");
        return false;
      }
    } catch (error) {
      setIsValidated(false);
      setValidationError(
        error instanceof Error ? error.message : "Validation failed"
      );
      return false;
    } finally {
      setIsValidating(false);
    }
  };

  // Save API key and advance
  const handleSave = async () => {
    if (!selectedProvider) return;

    const providerData = providers.find((p) => p.id === selectedProvider);
    if (providerData?.requires_api_key && !apiKey.trim()) return;

    if (providerData?.requires_api_key && !isValidated) {
      const valid = await validateKey();
      if (!valid) return;
    }

    setIsSaving(true);
    try {
      await saveProviderCredentials(selectedProvider, apiKey);
      nextStep();
    } catch (error) {
      setIsValidated(false);
      setValidationError(
        error instanceof Error ? error.message : "Failed to save credentials"
      );
    } finally {
      setIsSaving(false);
    }
  };

  // OAuth flow and advance
  const handleOAuth = async () => {
    if (!selectedProvider) return;

    setIsOAuthInProgress(true);
    setValidationError(null);
    try {
      const result = await startProviderOAuth(selectedProvider);
      if (result.success) {
        nextStep();
      } else {
        setValidationError(result.error ?? "OAuth flow failed");
      }
    } catch (error) {
      setValidationError(
        error instanceof Error ? error.message : "OAuth flow failed"
      );
    } finally {
      setIsOAuthInProgress(false);
    }
  };

  const selectedProviderData = providers.find((p) => p.id === selectedProvider);
  const busy = isSaving || isOAuthInProgress;

  if (isLoading) {
    return (
      <div className="flex flex-col items-center gap-4 text-center">
        <Loader2Icon className="h-8 w-8 animate-spin text-primary" />
        <p className="text-sm text-muted-foreground">Loading providers...</p>
      </div>
    );
  }

  if (loadError) {
    return (
      <div className="flex flex-col items-center gap-4 text-center">
        <AlertCircleIcon className="h-12 w-12 text-destructive" />
        <div>
          <h2 className="text-xl font-semibold">Failed to Load Providers</h2>
          <p className="text-sm text-muted-foreground">{loadError}</p>
        </div>
        <Button onClick={loadProviders} variant="outline">
          Retry
        </Button>
      </div>
    );
  }

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
          <Select value={selectedProvider} onValueChange={handleProviderChange}>
            <SelectTrigger className="w-full">
              <SelectValue placeholder="Select a provider" />
            </SelectTrigger>
            <SelectContent>
              {providers.map((provider) => (
                <SelectItem key={provider.id} value={provider.id}>
                  <div className="flex flex-col items-start">
                    <span>{provider.name}</span>
                    {provider.description && (
                      <span className="text-xs text-muted-foreground">
                        {provider.description}
                      </span>
                    )}
                  </div>
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        {selectedProviderData?.oauth_supported && (
          <Button
            onClick={() => void handleOAuth()}
            disabled={busy}
            className="w-full"
          >
            {isOAuthInProgress ? (
              <>
                <Loader2Icon className="mr-2 h-4 w-4 animate-spin" />
                Waiting for browser...
              </>
            ) : (
              `Connect with OAuth`
            )}
          </Button>
        )}

        {selectedProviderData?.oauth_supported && selectedProviderData?.requires_api_key && (
          <div className="flex items-center gap-3 text-xs text-muted-foreground">
            <div className="h-px flex-1 bg-border" />
            <span>or enter an API key</span>
            <div className="h-px flex-1 bg-border" />
          </div>
        )}

        {selectedProviderData?.requires_api_key && (
          <Field>
            <FieldLabel>API Key</FieldLabel>
            <div className="flex gap-2">
              <Input
                type="password"
                placeholder="Enter your API key"
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  if (isValidated) {
                    setIsValidated(false);
                    setValidationError(null);
                  }
                }}
                className="flex-1"
              />
              <Button
                onClick={() => void validateKey()}
                disabled={!apiKey.trim() || isValidating || busy}
                variant="secondary"
              >
                {isValidating ? (
                  <Loader2Icon className="h-4 w-4 animate-spin" />
                ) : (
                  "Validate"
                )}
              </Button>
            </div>
            <FieldDescription>
              Your API key will be securely stored and never shared.
            </FieldDescription>
            {isValidated && (
              <div className="flex items-center gap-2 text-sm text-green-500">
                <CheckCircle2Icon className="h-4 w-4" />
                <span>API key is valid</span>
              </div>
            )}
            {validationError && (
              <FieldError>{validationError}</FieldError>
            )}
          </Field>
        )}

        {selectedProviderData && !selectedProviderData.oauth_supported && (
          <Button
            onClick={() => void handleSave()}
            disabled={
              !selectedProvider ||
              (selectedProviderData.requires_api_key && !apiKey.trim()) ||
              busy
            }
            className="w-full"
          >
            {isSaving ? (
              <>
                <Loader2Icon className="mr-2 h-4 w-4 animate-spin" />
                Saving...
              </>
            ) : (
              "Continue"
            )}
          </Button>
        )}

        {selectedProviderData?.oauth_supported && selectedProviderData?.requires_api_key && apiKey.trim() && (
          <Button
            variant="outline"
            onClick={() => void handleSave()}
            disabled={busy}
            className="w-full"
          >
            {isSaving ? (
              <>
                <Loader2Icon className="mr-2 h-4 w-4 animate-spin" />
                Saving...
              </>
            ) : (
              "Save API Key & Continue"
            )}
          </Button>
        )}
      </div>
    </div>
  );
}
