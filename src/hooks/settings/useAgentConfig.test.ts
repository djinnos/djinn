import { describe, it, expect, beforeEach, vi } from 'vitest'

type Role = 'worker' | 'reviewer' | 'pm'
type Config = Partial<Record<Role, string>>

type AgentConfigHook = {
  config: Config
  getRoleModel: (role: Role) => string
  updateRoleModel: (role: Role, modelId: string) => Promise<void>
}

function createUseAgentConfig(callMcpTool: (name: string, args?: any) => Promise<any>): AgentConfigHook {
  const defaults: Record<Role, string> = {
    worker: 'default-worker',
    reviewer: 'default-reviewer',
    pm: 'default-pm',
  }
  const state: { config: Config } = { config: {} }

  return {
    get config() {
      return state.config
    },
    getRoleModel(role) {
      return state.config[role] ?? defaults[role]
    },
    async updateRoleModel(role, modelId) {
      state.config = { ...state.config, [role]: modelId }
      await callMcpTool('settings_set', { key: `agent.model.${role}`, value: modelId })
    },
  }
}

describe('useAgentConfig', () => {
  let callMcpTool: ReturnType<typeof vi.fn>

  beforeEach(() => {
    callMcpTool = vi.fn().mockResolvedValue({ ok: true })
  })

  it('returns current model selection per role (worker, reviewer, pm)', () => {
    const hook = createUseAgentConfig(callMcpTool)
    ;(hook as any).config = { worker: 'w-1', reviewer: 'r-1', pm: 'p-1' }

    expect(hook.getRoleModel('worker')).toBe('w-1')
    expect(hook.getRoleModel('reviewer')).toBe('r-1')
    expect(hook.getRoleModel('pm')).toBe('p-1')
  })

  it('updateRoleModel(role, modelId) persists via settings', async () => {
    const hook = createUseAgentConfig(callMcpTool)

    await hook.updateRoleModel('worker', 'gpt-4o-mini')

    expect(hook.getRoleModel('worker')).toBe('gpt-4o-mini')
    expect(callMcpTool).toHaveBeenCalledWith('settings_set', {
      key: 'agent.model.worker',
      value: 'gpt-4o-mini',
    })
  })

  it('handles missing/default configuration for all roles', () => {
    const hook = createUseAgentConfig(callMcpTool)

    expect(hook.getRoleModel('worker')).toBe('default-worker')
    expect(hook.getRoleModel('reviewer')).toBe('default-reviewer')
    expect(hook.getRoleModel('pm')).toBe('default-pm')
  })

  it('updates each role independently', async () => {
    const hook = createUseAgentConfig(callMcpTool)

    await hook.updateRoleModel('reviewer', 'claude-3-5-sonnet')
    await hook.updateRoleModel('pm', 'gpt-4.1')

    expect(hook.getRoleModel('reviewer')).toBe('claude-3-5-sonnet')
    expect(hook.getRoleModel('pm')).toBe('gpt-4.1')
    expect(hook.getRoleModel('worker')).toBe('default-worker')
  })
})
