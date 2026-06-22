import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, cleanup, within } from '@testing-library/react';
import { ConnectionStatus } from './ConnectionStatus';
import { sseStore } from '@/stores/sseStore';

vi.mock('@/lib/toast', () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

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
