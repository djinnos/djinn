type UnlistenFn = () => void;

export async function listen<T>(
  event: string,
  handler: (event: { payload: T }) => void
): Promise<UnlistenFn> {
  if (typeof window === "undefined" || typeof window.electronAPI?.on !== "function") {
    void event;
    void handler;
    return () => undefined;
  }
  return window.electronAPI.on(event, (payload: unknown) =>
    handler({ payload: payload as T })
  );
}
