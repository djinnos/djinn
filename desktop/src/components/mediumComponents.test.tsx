import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor, cleanup, within } from '@testing-library/react';
import { AgentConfig } from './AgentConfig';
import { ConnectionStatus } from './ConnectionStatus';
import { SyncHealthBanner } from './SyncHealthBanner';
import { sseStore } from '@/stores/sseStore';

vi.mock('@/lib/toast', () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock('@/api/mcpClient', () => ({
  callMcpTool: vi.fn(),
}));

import { callMcpTool } from '@/api/mcpClient';

describe('AgentConfig', () => {
  afterEach(() => cleanup());

  it('renders header, model picker, and current model entry', () => {
    const { container } = render(
      <AgentConfig
        models={[
          {
            provider: 'openai',
            model: 'gpt-4o',
            max_concurrent: 2,
          },
        ]}
        availableModels={[
          { provider_id: 'openai', id: 'gpt-4o-mini', name: 'GPT 4o Mini', tool_call: true, reasoning: false, attachment: false, context_window: 128000, output_limit: 16384, pricing: { input_per_m: 0, output_per_m: 0 } },
          { provider_id: 'anthropic', id: 'claude-3-5-sonnet', name: 'Claude Sonnet', tool_call: true, reasoning: false, attachment: false, context_window: 128000, output_limit: 16384, pricing: { input_per_m: 0, output_per_m: 0 } },
        ]}
        isLoading={false}
        isSaving={false}
        error={null}
        hasUnsavedChanges={false}
        onAddModel={vi.fn()}
        onRemoveModel={vi.fn()}
        onReorderModels={vi.fn()}
        onUpdateMaxSessions={vi.fn()}
        onDismissError={vi.fn()}
        onSave={vi.fn()}
      />,
    );

    const root = container.firstElementChild as HTMLElement;
    expect(within(root).getByRole('heading', { name: 'Models' })).toBeInTheDocument();

    expect(within(root).getByRole('button', { name: /Add Model/i })).toBeInTheDocument();

    expect(within(root).getByText('gpt-4o')).toBeInTheDocument();
    expect(within(root).getByText('OpenAI')).toBeInTheDocument();
  });

  it('shows empty state when no models are configured', () => {
    const { container } = render(
      <AgentConfig
        models={[]}
        availableModels={[
          { provider_id: 'openai', id: 'openai/gpt-5.4', name: 'GPT 5.4', tool_call: true, reasoning: false, attachment: false, context_window: 128000, output_limit: 16384, pricing: { input_per_m: 0, output_per_m: 0 } },
        ]}
        isLoading={false}
        isSaving={false}
        error={null}
        hasUnsavedChanges={false}
        onAddModel={vi.fn()}
        onRemoveModel={vi.fn()}
        onReorderModels={vi.fn()}
        onUpdateMaxSessions={vi.fn()}
        onDismissError={vi.fn()}
        onSave={vi.fn()}
      />,
    );

    const root = container.firstElementChild as HTMLElement;
    expect(within(root).getByText('No models configured. Use the Add model button to get started.')).toBeInTheDocument();
  });

  it('disables save while saving', () => {
    render(
      <AgentConfig
        models={[]}
        availableModels={[]}
        isLoading={false}
        isSaving={true}
        error={null}
        hasUnsavedChanges={true}
        onAddModel={vi.fn()}
        onRemoveModel={vi.fn()}
        onReorderModels={vi.fn()}
        onUpdateMaxSessions={vi.fn()}
        onDismissError={vi.fn()}
        onSave={vi.fn()}
      />,
    );

    expect(screen.getByRole('button', { name: 'Saving...' })).toBeDisabled();
  });

  const baseProps = {
    isLoading: false,
    isSaving: false,
    error: null,
    hasUnsavedChanges: false,
    onAddModel: vi.fn(),
    onRemoveModel: vi.fn(),
    onReorderModels: vi.fn(),
    onUpdateMaxSessions: vi.fn(),
    onDismissError: vi.fn(),
    onSave: vi.fn(),
  };

  const makePricingModel = (overrides: Partial<{ id: string; name: string; provider_id: string }>) => ({
    id: 'gpt-4o',
    name: 'GPT-4o',
    provider_id: 'openai',
    attachment: false,
    context_window: 128000,
    output_limit: 16384,
    pricing: { input_per_million: 2.5, output_per_million: 10, cache_read_per_million: 1.25, cache_write_per_million: 0 },
    reasoning: false,
    tool_call: true,
    ...overrides,
  });

  it('shows pretty model name when model id matches without provider prefix', () => {
    render(
      <AgentConfig
        {...baseProps}
        models={[{ model: 'gpt-4o', provider: 'openai', max_concurrent: 1 }]}
        availableModels={[makePricingModel({ id: 'gpt-4o', name: 'GPT-4o', provider_id: 'openai' })]}
      />,
    );
    expect(screen.getByText('GPT-4o')).toBeInTheDocument();
  });

  it('shows pretty model name when availableModels id includes provider prefix', () => {
    // Backend returns ids like "openai/gpt-5.4" but entry.model is "gpt-5.4" after splitModelId
    render(
      <AgentConfig
        {...baseProps}
        models={[{ model: 'gpt-5.4', provider: 'openai', max_concurrent: 1 }]}
        availableModels={[makePricingModel({ id: 'openai/gpt-5.4', name: 'GPT-5.4', provider_id: 'openai' })]}
      />,
    );
    expect(screen.getByText('GPT-5.4')).toBeInTheDocument();
    expect(screen.queryByText('gpt-5.4')).not.toBeInTheDocument();
  });

  it('shows pretty name when entry.model itself has provider prefix (in-session before save)', () => {
    // Before save+reload, entry.model may still be the full "openai/gpt-5.3-codex" id
    // The display should still resolve to the pretty name
    render(
      <AgentConfig
        {...baseProps}
        models={[{ model: 'openai/gpt-5.3-codex', provider: 'openai', max_concurrent: 1 }]}
        availableModels={[makePricingModel({ id: 'openai/gpt-5.3-codex', name: 'GPT-5.3 Codex', provider_id: 'openai' })]}
      />,
    );
    expect(screen.getByText('GPT-5.3 Codex')).toBeInTheDocument();
    expect(screen.queryByText('openai/gpt-5.3-codex')).not.toBeInTheDocument();
  });

  it('falls back to raw model id when not found in availableModels', () => {
    render(
      <AgentConfig
        {...baseProps}
        models={[{ model: 'unknown-model', provider: 'openai', max_concurrent: 1 }]}
        availableModels={[]}
      />,
    );
    expect(screen.getByText('unknown-model')).toBeInTheDocument();
  });
});

describe('ConnectionStatus', () => {
  beforeEach(() => {
    sseStore.setState({ connectionStatus: 'connected', reconnectAttempt: 0, lastError: null, isConnected: true });
  });

  afterEach(() => {
    cleanup();
    sseStore.setState({ connectionStatus: 'connected', reconnectAttempt: 0, lastError: null, isConnected: true });
  });

  it('renders connected state', () => {
    sseStore.setState({ connectionStatus: 'connected', reconnectAttempt: 0, lastError: null, isConnected: true });
    const { container } = render(<ConnectionStatus />);
    const statusRoot = container.firstElementChild as HTMLElement;
    expect(statusRoot).toHaveAttribute('title', expect.stringContaining('Connected'));
    expect(within(statusRoot).getByText('Connected')).toBeInTheDocument();
  });

  it('renders reconnecting state', () => {
    sseStore.setState({ connectionStatus: 'reconnecting', reconnectAttempt: 1, lastError: null, isConnected: false });
    const { container } = render(<ConnectionStatus />);
    const statusRoot = container.firstElementChild as HTMLElement;
    expect(statusRoot).toHaveAttribute('title', expect.stringContaining('Reconnecting'));
    expect(within(statusRoot).getByText('Reconnecting')).toBeInTheDocument();
  });

  it('renders disconnected/error state', () => {
    sseStore.setState({ connectionStatus: 'error', reconnectAttempt: 0, lastError: new Error('boom'), isConnected: false });
    const { container } = render(<ConnectionStatus />);
    const statusRoot = container.firstElementChild as HTMLElement;
    expect(statusRoot).toHaveAttribute('title', expect.stringContaining('Connection Error'));
    expect(within(statusRoot).getByText('Connection Error')).toBeInTheDocument();
  });
});

describe('SyncHealthBanner', () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
    sseStore.setState({ connectionStatus: 'connected', reconnectAttempt: 0, lastError: null, isConnected: true });
  });

  afterEach(() => {
    cleanup();
    vi.mocked(callMcpTool).mockReset();
    sseStore.setState({ connectionStatus: 'connected', reconnectAttempt: 0, lastError: null, isConnected: true });
  });

  it('is hidden when healthy', async () => {
    vi.mocked(callMcpTool).mockResolvedValue({ channels: [{ failure_count: 0, last_error: null }] });
    render(<SyncHealthBanner />);

    await waitFor(() => {
      expect(screen.queryByText('Sync Issues Detected')).not.toBeInTheDocument();
    });
  });

  it('shows warning on errors', async () => {
    vi.mocked(callMcpTool).mockResolvedValue({ channels: [{ failure_count: 3, last_error: 'fatal: auth failed' }] });
    render(<SyncHealthBanner />);

    expect(await screen.findByText('Sync Issues Detected')).toBeInTheDocument();
    expect(screen.getByText('fatal: auth failed')).toBeInTheDocument();
  });
});
