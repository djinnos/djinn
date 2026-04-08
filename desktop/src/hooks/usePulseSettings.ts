import { useCallback, useEffect, useMemo, useState } from "react";

// Pulse calibration settings live in localStorage scoped per project path.
// The MCP `project_config_*` tools only accept a fixed set of keys
// (target_branch, auto_merge, etc.), so they can't store arbitrary
// `pulse.*` calibration data without server changes — which is out of
// scope for Phase 3. Localstorage keeps the calibration on the box where
// the user calibrated it, which matches the "honest, per-developer"
// shape of these settings anyway.

export interface PulseSettings {
  excluded_paths: string[];
  orphan_ignore: string[];
}

const EMPTY: PulseSettings = { excluded_paths: [], orphan_ignore: [] };

function storageKey(projectPath: string): string {
  return `djinn:pulse:settings:${projectPath}`;
}

function read(projectPath: string | null): PulseSettings {
  if (!projectPath) return EMPTY;
  try {
    const raw = localStorage.getItem(storageKey(projectPath));
    if (!raw) return EMPTY;
    const parsed = JSON.parse(raw) as Partial<PulseSettings>;
    return {
      excluded_paths: Array.isArray(parsed.excluded_paths)
        ? parsed.excluded_paths.filter((s): s is string => typeof s === "string")
        : [],
      orphan_ignore: Array.isArray(parsed.orphan_ignore)
        ? parsed.orphan_ignore.filter((s): s is string => typeof s === "string")
        : [],
    };
  } catch {
    return EMPTY;
  }
}

function write(projectPath: string, value: PulseSettings) {
  try {
    localStorage.setItem(storageKey(projectPath), JSON.stringify(value));
  } catch {
    // ignore quota errors
  }
}

const STORAGE_EVENT = "djinn:pulse:settings-changed";

export function usePulseSettings(projectPath: string | null) {
  const [settings, setSettings] = useState<PulseSettings>(() => read(projectPath));

  useEffect(() => {
    setSettings(read(projectPath));
  }, [projectPath]);

  useEffect(() => {
    function handler(e: Event) {
      const detail = (e as CustomEvent<{ project: string }>).detail;
      if (detail?.project === projectPath) {
        setSettings(read(projectPath));
      }
    }
    window.addEventListener(STORAGE_EVENT, handler);
    return () => window.removeEventListener(STORAGE_EVENT, handler);
  }, [projectPath]);

  const update = useCallback(
    (next: PulseSettings) => {
      if (!projectPath) return;
      write(projectPath, next);
      setSettings(next);
      window.dispatchEvent(
        new CustomEvent(STORAGE_EVENT, { detail: { project: projectPath } })
      );
    },
    [projectPath]
  );

  const addExcludedPath = useCallback(
    (pattern: string) => {
      const trimmed = pattern.trim();
      if (!trimmed || settings.excluded_paths.includes(trimmed)) return;
      update({ ...settings, excluded_paths: [...settings.excluded_paths, trimmed] });
    },
    [settings, update]
  );

  const removeExcludedPath = useCallback(
    (pattern: string) => {
      update({
        ...settings,
        excluded_paths: settings.excluded_paths.filter((p) => p !== pattern),
      });
    },
    [settings, update]
  );

  const addOrphanIgnore = useCallback(
    (path: string) => {
      const trimmed = path.trim();
      if (!trimmed || settings.orphan_ignore.includes(trimmed)) return;
      update({ ...settings, orphan_ignore: [...settings.orphan_ignore, trimmed] });
    },
    [settings, update]
  );

  const removeOrphanIgnore = useCallback(
    (path: string) => {
      update({
        ...settings,
        orphan_ignore: settings.orphan_ignore.filter((p) => p !== path),
      });
    },
    [settings, update]
  );

  return useMemo(
    () => ({
      settings,
      addExcludedPath,
      removeExcludedPath,
      addOrphanIgnore,
      removeOrphanIgnore,
    }),
    [settings, addExcludedPath, removeExcludedPath, addOrphanIgnore, removeOrphanIgnore]
  );
}
