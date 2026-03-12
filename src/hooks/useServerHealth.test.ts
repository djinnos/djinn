import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'

type HealthHook = {
  connected: boolean
  start: () => void
  stop: () => void
}

function createUseServerHealth(checkConnection: () => Promise<boolean>, intervalMs = 1000): HealthHook {
  const state = { connected: false }
  let timer: any

  const poll = async () => {
    try {
      state.connected = await checkConnection()
    } catch {
      state.connected = false
    }
  }

  return {
    get connected() {
      return state.connected
    },
    start() {
      void poll()
      timer = setInterval(() => void poll(), intervalMs)
    },
    stop() {
      if (timer) clearInterval(timer)
    },
  }
}

describe('useServerHealth', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('returns connected/disconnected status', async () => {
    const checkConnection = vi.fn().mockResolvedValueOnce(true).mockResolvedValueOnce(false)
    const hook = createUseServerHealth(checkConnection, 500)

    hook.start()
    await vi.runOnlyPendingTimersAsync()
    expect(hook.connected).toBe(true)

    await vi.advanceTimersByTimeAsync(500)
    expect(hook.connected).toBe(false)

    hook.stop()
  })

  it('polls on interval and updates state', async () => {
    const checkConnection = vi
      .fn()
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(true)
    const hook = createUseServerHealth(checkConnection, 200)

    hook.start()
    await vi.runOnlyPendingTimersAsync()
    expect(hook.connected).toBe(false)

    await vi.advanceTimersByTimeAsync(200)
    expect(hook.connected).toBe(true)

    await vi.advanceTimersByTimeAsync(200)
    expect(checkConnection).toHaveBeenCalledTimes(3)
    expect(hook.connected).toBe(true)

    hook.stop()
  })

  it('handles connection failure gracefully', async () => {
    const checkConnection = vi.fn().mockRejectedValue(new Error('offline'))
    const hook = createUseServerHealth(checkConnection, 250)

    hook.start()
    await vi.runOnlyPendingTimersAsync()

    expect(hook.connected).toBe(false)
    hook.stop()
  })
})
