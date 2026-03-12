import { describe, it, expect, beforeEach, vi } from 'vitest'

type ProviderModel = { id: string; name: string }
type Provider = { id: string; name: string; models: ProviderModel[] }

type HookState = {
  catalog: Provider[]
  error: string | null
  fetchCatalog: () => Promise<Provider[]>
  saveCredentials: (providerId: string, apiKey: string) => Promise<void>
  removeProvider: (providerId: string) => Promise<void>
  startOAuth: (providerId: string) => Promise<string>
  validateKey: (providerId: string, apiKey: string) => Promise<boolean>
}

function createUseProviders(callMcpTool: (name: string, args?: any) => Promise<any>): HookState {
  const state: { catalog: Provider[]; error: string | null } = { catalog: [], error: null }

  return {
    get catalog() {
      return state.catalog
    },
    get error() {
      return state.error
    },
    async fetchCatalog() {
      const result = await callMcpTool('provider_catalog')
      state.catalog = result.providers ?? []
      return state.catalog
    },
    async saveCredentials(providerId, apiKey) {
      await callMcpTool('credential_set', { providerId, apiKey })
    },
    async removeProvider(providerId) {
      await callMcpTool('credential_delete', { providerId })
      await callMcpTool('provider_remove', { providerId })
    },
    async startOAuth(providerId) {
      const result = await callMcpTool('provider_oauth_start', { providerId })
      return result.authUrl
    },
    async validateKey(providerId, apiKey) {
      state.error = null
      try {
        const result = await callMcpTool('provider_validate', { providerId, apiKey })
        return !!result.valid
      } catch (err: any) {
        state.error = err?.message ?? 'Validation failed'
        return false
      }
    },
  }
}

describe('useProviders', () => {
  let callMcpTool: ReturnType<typeof vi.fn>

  beforeEach(() => {
    callMcpTool = vi.fn()
  })

  it('fetchCatalog() returns provider list with models', async () => {
    const providers = [{ id: 'openai', name: 'OpenAI', models: [{ id: 'gpt-4o', name: 'GPT-4o' }] }]
    callMcpTool.mockResolvedValue({ providers })
    const hook = createUseProviders(callMcpTool)

    const result = await hook.fetchCatalog()

    expect(callMcpTool).toHaveBeenCalledWith('provider_catalog')
    expect(result).toEqual(providers)
    expect(hook.catalog).toEqual(providers)
  })

  it('saveCredentials(providerId, apiKey) calls credential_set', async () => {
    callMcpTool.mockResolvedValue({ ok: true })
    const hook = createUseProviders(callMcpTool)

    await hook.saveCredentials('openai', 'sk-test')

    expect(callMcpTool).toHaveBeenCalledWith('credential_set', { providerId: 'openai', apiKey: 'sk-test' })
  })

  it('removeProvider(providerId) calls credential_delete + provider_remove', async () => {
    callMcpTool.mockResolvedValue({ ok: true })
    const hook = createUseProviders(callMcpTool)

    await hook.removeProvider('anthropic')

    expect(callMcpTool).toHaveBeenNthCalledWith(1, 'credential_delete', { providerId: 'anthropic' })
    expect(callMcpTool).toHaveBeenNthCalledWith(2, 'provider_remove', { providerId: 'anthropic' })
  })

  it('startOAuth(providerId) calls provider_oauth_start, returns auth URL', async () => {
    callMcpTool.mockResolvedValue({ authUrl: 'https://auth.example/start' })
    const hook = createUseProviders(callMcpTool)

    const url = await hook.startOAuth('github')

    expect(callMcpTool).toHaveBeenCalledWith('provider_oauth_start', { providerId: 'github' })
    expect(url).toBe('https://auth.example/start')
  })

  it('validateKey(providerId, apiKey) calls provider_validate', async () => {
    callMcpTool.mockResolvedValue({ valid: true })
    const hook = createUseProviders(callMcpTool)

    const valid = await hook.validateKey('openai', 'sk-abc')

    expect(callMcpTool).toHaveBeenCalledWith('provider_validate', { providerId: 'openai', apiKey: 'sk-abc' })
    expect(valid).toBe(true)
    expect(hook.error).toBeNull()
  })

  it('sets error state on failed validation', async () => {
    callMcpTool.mockRejectedValue(new Error('Invalid API key'))
    const hook = createUseProviders(callMcpTool)

    const valid = await hook.validateKey('openai', 'bad-key')

    expect(valid).toBe(false)
    expect(hook.error).toBe('Invalid API key')
  })
})
