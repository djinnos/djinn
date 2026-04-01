type UnlistenFn = () => void;

export async function listen<T>(
  event: string,
  handler: (event: { payload: T }) => void
): Promise<UnlistenFn> {
  return window.electronAPI.on(event, (payload: unknown) =>
    handler({ payload: payload as T })
  );
}
