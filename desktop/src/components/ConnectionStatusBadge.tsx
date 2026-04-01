import { useState, useEffect } from 'react';
import { useNavigate } from 'react-router-dom';
import { useServerHealth } from '@/hooks/useServerHealth';
import { getConnectionMode, getTunnelStatus, getSshHosts, type ConnectionMode, type TunnelStatus, type SshHost } from '@/tauri/commands';
import { listen } from '@tauri-apps/api/event';
import { cn } from '@/lib/utils';

export function ConnectionStatusBadge() {
  const navigate = useNavigate();
  const { status: healthStatus } = useServerHealth();
  const [mode, setMode] = useState<ConnectionMode>({ type: 'daemon' });
  const [tunnelStatus, setTunnelStatus] = useState<TunnelStatus>({ status: 'disconnected' });
  const [hostLabel, setHostLabel] = useState<string | null>(null);

  useEffect(() => {
    void getConnectionMode().then((m) => { if (m) setMode(m); }).catch(() => {});
  }, []);

  useEffect(() => {
    if (mode.type === 'ssh') {
      void getTunnelStatus().then(setTunnelStatus).catch(() => {});
      void getSshHosts().then((hosts: SshHost[]) => {
        const hostId = (mode as { type: 'ssh'; host_id: string }).host_id;
        const host = hosts.find((h) => h.id === hostId);
        setHostLabel(host?.label ?? null);
      }).catch(() => {});
    }
  }, [mode]);

  useEffect(() => {
    const unlisten = listen<TunnelStatus>('tunnel:status-changed', (event) => {
      setTunnelStatus(event.payload);
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  const isHealthy = healthStatus === 'connected';

  let label: string;
  let dotColor: string;

  switch (mode.type) {
    case 'daemon':
      label = 'Local';
      dotColor = isHealthy ? 'bg-emerald-400' : 'bg-zinc-500';
      break;
    case 'ssh':
      label = hostLabel ? `SSH: ${hostLabel}` : 'SSH';
      if (tunnelStatus.status === 'connected' && isHealthy) {
        dotColor = 'bg-emerald-400';
      } else if (tunnelStatus.status === 'connecting' || tunnelStatus.status === 'reconnecting') {
        dotColor = 'bg-yellow-400';
      } else if (tunnelStatus.status === 'error') {
        dotColor = 'bg-red-500';
      } else {
        dotColor = 'bg-zinc-500';
      }
      break;
    case 'wsl':
      label = 'WSL';
      dotColor = isHealthy ? 'bg-emerald-400' : 'bg-zinc-500';
      break;
    case 'remote':
      label = 'Remote';
      dotColor = isHealthy ? 'bg-emerald-400' : 'bg-zinc-500';
      break;
    default:
      label = 'Local';
      dotColor = 'bg-zinc-500';
  }

  return (
    <button
      type="button"
      onClick={() => navigate('/settings')}
      className={cn(
        'flex items-center gap-2 rounded-md px-2.5 py-1.5 text-xs text-muted-foreground',
        'transition-colors hover:bg-white/[0.04] hover:text-foreground',
      )}
      title="Connection settings"
    >
      <span className={cn('inline-block h-2 w-2 rounded-full shrink-0', dotColor)} />
      <span className="truncate">{label}</span>
    </button>
  );
}
