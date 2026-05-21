import { useCallback, useEffect, useState } from "react";
import {
  fetchUserSettings,
  patchUserSettings,
  type UserSettings,
} from "@/api/userSettings";

interface State {
  settings: UserSettings | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
}

export function useUserSettings() {
  const [state, setState] = useState<State>({
    settings: null,
    loading: true,
    saving: false,
    error: null,
  });

  const load = useCallback(async () => {
    setState((s) => ({ ...s, loading: true, error: null }));
    try {
      const settings = await fetchUserSettings();
      setState({ settings, loading: false, saving: false, error: null });
    } catch (e) {
      setState((s) => ({
        ...s,
        loading: false,
        error: e instanceof Error ? e.message : String(e),
      }));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const setAutoApprovePrs = useCallback(async (value: boolean) => {
    setState((s) => ({ ...s, saving: true, error: null }));
    try {
      const next = await patchUserSettings({ autoApprovePrs: value });
      setState({ settings: next, loading: false, saving: false, error: null });
    } catch (e) {
      setState((s) => ({
        ...s,
        saving: false,
        error: e instanceof Error ? e.message : String(e),
      }));
    }
  }, []);

  return {
    settings: state.settings,
    loading: state.loading,
    saving: state.saving,
    error: state.error,
    reload: load,
    setAutoApprovePrs,
  };
}
