export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (args !== undefined) {
    return window.electronAPI.invoke(cmd, args) as Promise<T>;
  }
  return window.electronAPI.invoke(cmd) as Promise<T>;
}
