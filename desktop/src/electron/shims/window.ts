export function getCurrentWindow() {
  if (typeof window === "undefined" || typeof window.electronAPI?.getWindow !== "function") {
    return {
      minimize: async () => undefined,
      toggleMaximize: async () => undefined,
      close: async () => undefined,
      startDragging: () => undefined,
    };
  }
  return window.electronAPI.getWindow();
}
