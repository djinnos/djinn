interface ElectronAPI {
  invoke: (channel: string, args?: Record<string, unknown>) => Promise<unknown>;
  on: (event: string, callback: (payload: unknown) => void) => Promise<() => void>;
  getWindow: () => {
    minimize: () => Promise<void>;
    toggleMaximize: () => Promise<void>;
    close: () => Promise<void>;
    startDragging: () => void;
  };
}

interface Window {
  electronAPI: ElectronAPI;
}
