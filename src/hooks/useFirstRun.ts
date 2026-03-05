import { useState, useEffect } from "react";
import { fetchProjects, fetchProviderConfigStatus } from "@/api/server";

export interface FirstRunState {
  isFirstRun: boolean | null;
  isLoading: boolean;
  error: string | null;
}

/**
 * Hook to detect if this is the first run of the application.
 * First run is determined by:
 * - No projects registered AND
 * - No providers configured
 */
export function useFirstRun(): FirstRunState {
  const [isFirstRun, setIsFirstRun] = useState<boolean | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const checkFirstRun = async () => {
      setIsLoading(true);
      setError(null);
      try {
        // Check for projects
        const projects = await fetchProjects();
        const hasProjects = projects.length > 0;

        // Check for configured providers
        const providerStatus = await fetchProviderConfigStatus();
        const hasProviders = providerStatus.configured;

        // First run if no projects AND no providers
        setIsFirstRun(!hasProjects && !hasProviders);
      } catch (err) {
        setError(err instanceof Error ? err.message : "Failed to check first-run status");
        // On error, assume first run to show the wizard
        setIsFirstRun(true);
      } finally {
        setIsLoading(false);
      }
    };

    checkFirstRun();
  }, []);

  return { isFirstRun, isLoading, error };
}
