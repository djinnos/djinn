import { contextBridge, ipcRenderer, IpcRendererEvent } from 'electron';

contextBridge.exposeInMainWorld('electronAPI', {
  invoke: (channel: string, args?: Record<string, unknown>): Promise<unknown> => {
    return ipcRenderer.invoke(channel, args);
  },

  on: (event: string, callback: (payload: unknown) => void): Promise<() => void> => {
    const handler = (_event: IpcRendererEvent, payload: unknown) => callback(payload);
    ipcRenderer.on(event, handler);
    // Return a promise that resolves to an unlisten function
    return Promise.resolve(() => {
      ipcRenderer.removeListener(event, handler);
    });
  },

  getWindow: () => ({
    minimize: () => ipcRenderer.invoke('window:minimize'),
    toggleMaximize: () => ipcRenderer.invoke('window:toggleMaximize'),
    close: () => ipcRenderer.invoke('window:close'),
    startDragging: () => {
      // No-op: handled by CSS -webkit-app-region: drag
    },
  }),
});
