import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor, cleanup, within } from '@testing-library/react';
import { AgentConfig } from './AgentConfig';
import { ConnectionStatus } from './ConnectionStatus';
import { GitRemoteSetupBanner } from './GitRemoteSetupBanner';
import { SyncHealthBanner } from './SyncHealthBanner';
import { sseStore } from '@/stores/sseStore';

vi.mock('@/tauri/commands', () => ({
  checkGitRemote: vi.fn(),
  setupGitRemote: vi.fn(),
}));

vi.mock('@/lib/toast', () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock('@/api/mcpClient', () => ({
  callMcpTool: vi.fn(),
}));

import { callMcpTool } from '@/api/mcpClient';

describe('AgentConfig', () => {
  afterEach(() => cleanup());

  it('renders role sections, model dropdown, and current selection', () => {
    const { container } = render(
      <AgentConfig
        models={[
          {
            provider: 'openai',
            model: 'gpt-4o',
            enabledRoles: ['worker', 'lead'],
            maxConcurrent: 2,
          },
        ]}
        availableModels={[
          { provider: 'openai', id: 'gpt-4o-mini', name: 'GPT 4o Mini' },
          { provider: 'anthropic', id: 'claude-3-5-sonnet', name: 'Claude Sonnet' },
        ]}
        isLoading={false}
        isSaving={false}
        error={null}
        hasUnsavedChanges={false}
        onAddModel={vi.fn()}
        onRemoveModel={vi.fn()}
        onReorderModels={vi.fn()}
        onToggleRole={vi.fn()}
        onUpdateMaxSessions={vi.fn()}
        onDismissError={vi.fn()}
        onSave={vi.fn()}
      />,
    );

    const root = container.firstElementChild as HTMLElement;
    expect(within(root).getByRole('heading', { name: 'Model Configuration' })).toBeInTheDocument();

    const searchInput = within(root).getByPlaceholderText('Search models...');
    expect(searchInput).toBeInTheDocument();

    const legend = root.querySelector('.flex.items-center.gap-4.text-xs.text-muted-foreground') as HTMLElement;
    expect(within(legend).getByText((_, el) => el?.textContent === 'W = Worker')).toBeInTheDocument();
    expect(within(legend).getByText((_, el) => el?.textContent === 'R = Reviewer')).toBeInTheDocument();
    expect(within(legend).getByText((_, el) => el?.textContent === 'L = Lead')).toBeInTheDocument();

    expect(within(root).getByText('gpt-4o')).toBeInTheDocument();
    expect(within(root).getByText('openai')).toBeInTheDocument();
    expect(within(root).getByTitle('Disable for Worker')).toBeInTheDocument();
    expect(within(root).getByTitle('Disable for Lead')).toBeInTheDocument();

    fireEvent.focus(searchInput);
    expect(within(root).getByRole('button', { name: /GPT 4o Mini/i })).toBeInTheDocument();
    expect(within(root).getByRole('button', { name: /Claude Sonnet/i })).toBeInTheDocument();
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
        onToggleRole={vi.fn()}
        onUpdateMaxSessions={vi.fn()}
        onDismissError={vi.fn()}
        onSave={vi.fn()}
      />,
    );

    expect(screen.getByRole('button', { name: 'Saving...' })).toBeDisabled();
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

describe('GitRemoteSetupBanner', () => {
  beforeEach(() => {
    cleanup();
  });

  afterEach(() => {
    cleanup();
  });

  it('shows setup prompt and hides after dismiss in same render', async () => {
    const { container } = render(<GitRemoteSetupBanner projectPath="/tmp/proj" onResolved={vi.fn()} />);
    const root = container.firstElementChild as HTMLElement;

    expect(within(root).getByRole('heading', { name: 'Git Remote Required' })).toBeInTheDocument();
    expect(within(root).getByPlaceholderText('https://github.com/you/repo.git')).toBeInTheDocument();

    const dismissButton = within(root).getByRole('button', { name: /dismiss git remote setup banner/i });
    fireEvent.click(dismissButton);

    await waitFor(() => {
      expect(screen.queryByRole('heading', { name: 'Git Remote Required' })).not.toBeInTheDocument();
    });
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
