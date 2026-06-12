import { useEffect, useRef } from "react";
import type { ConnectionStatus } from "@/hooks/useServerHealth";
import { refreshDispatchPauseStatus } from "@/stores/dispatchPauseStore";

/** Hydrate dispatch-pause visibility state once the authenticated app reaches the server. */
export function useDispatchPauseHydration(status: ConnectionStatus): void {
  const hydratedForCurrentConnection = useRef(false);

  useEffect(() => {
    if (status !== "connected") {
      hydratedForCurrentConnection.current = false;
      return;
    }

    if (hydratedForCurrentConnection.current) return;
    hydratedForCurrentConnection.current = true;
    void refreshDispatchPauseStatus();
  }, [status]);
}
