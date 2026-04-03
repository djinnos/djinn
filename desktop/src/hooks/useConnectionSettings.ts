import { useState, useEffect, useCallback } from "react";
import { listen } from "@/electron/shims/event";
import {
  getConnectionMode,
  setConnectionMode,
  getSshHosts,
  saveSshHost,
  removeSshHost,
  testSshConnection,
  getTunnelStatus,
  deployServerToHost,
  checkWslAvailable,
  retryServerConnection,
  type ConnectionMode,
  type SshHost,
  type TunnelStatus,
} from "@/electron/commands";
import { showToast } from "@/lib/toast";

export function useConnectionSettings() {
  const [mode, setMode] = useState<ConnectionMode>({ type: "daemon" });
  const [hosts, setHosts] = useState<SshHost[]>([]);
  const [tunnelStatus, setTunnelStatus] = useState<TunnelStatus>({ status: "disconnected" });
  const [loading, setLoading] = useState(true);
  const [wslAvailable, setWslAvailable] = useState(false);

  const loadHosts = useCallback(async () => {
    try {
      const list = await getSshHosts();
      setHosts(list);
    } catch (err) {
      showToast.error("Failed to load SSH hosts", {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, []);

  const loadInitialData = useCallback(async () => {
    setLoading(true);
    try {
      const [currentMode, hostList, wsl] = await Promise.all([
        getConnectionMode(),
        getSshHosts(),
        checkWslAvailable().catch(() => false),
      ]);
      setMode(currentMode);
      setHosts(hostList);
      setWslAvailable(wsl);

      if (currentMode.type === "ssh") {
        try {
          const status = await getTunnelStatus();
          setTunnelStatus(status);
        } catch {
          // tunnel status not available yet
        }
      }
    } catch (err) {
      showToast.error("Failed to load connection settings", {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadInitialData();
  }, [loadInitialData]);

  // Listen for tunnel status events from Electron backend
  useEffect(() => {
    const unlisten = listen<TunnelStatus>("tunnel:status-changed", (event) => {
      setTunnelStatus(event.payload);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const switchMode = useCallback(async (newMode: ConnectionMode) => {
    try {
      await setConnectionMode(newMode);
      setMode(newMode);
    } catch (err) {
      showToast.error("Failed to save connection mode", {
        description: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    try {
      await retryServerConnection();
      showToast.success("Connection mode updated");
    } catch (err) {
      showToast.error("Failed to connect after switching mode", {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, []);

  const addHost = useCallback(async (host: SshHost) => {
    try {
      await saveSshHost(host);
      await loadHosts();
      showToast.success("SSH host saved");
    } catch (err) {
      showToast.error("Failed to save SSH host", {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, [loadHosts]);

  const editHost = useCallback(async (host: SshHost) => {
    try {
      await saveSshHost(host);
      await loadHosts();
      showToast.success("SSH host updated");
    } catch (err) {
      showToast.error("Failed to update SSH host", {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, [loadHosts]);

  const deleteHost = useCallback(async (id: string) => {
    try {
      await removeSshHost(id);
      await loadHosts();
      showToast.success("SSH host removed");
    } catch (err) {
      showToast.error("Failed to remove SSH host", {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, [loadHosts]);

  const testHost = useCallback(async (id: string): Promise<string> => {
    try {
      const result = await testSshConnection(id);
      return result;
    } catch (err) {
      throw err;
    }
  }, []);

  const connectToHost = useCallback(async (id: string) => {
    await switchMode({ type: "ssh", host_id: id });
  }, [switchMode]);

  const deployToHost = useCallback(async (id: string): Promise<string> => {
    try {
      const result = await deployServerToHost(id);
      await loadHosts();
      return result;
    } catch (err) {
      showToast.error("Failed to deploy server", {
        description: err instanceof Error ? err.message : String(err),
      });
      throw err;
    }
  }, [loadHosts]);

  return {
    mode,
    hosts,
    tunnelStatus,
    loading,
    wslAvailable,
    switchMode,
    addHost,
    editHost,
    deleteHost,
    testHost,
    connectToHost,
    deployToHost,
  };
}
