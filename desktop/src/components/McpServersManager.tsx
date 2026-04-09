import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ConfirmButton } from "@/components/ConfirmButton";
import { InlineError } from "@/components/InlineError";
import { useSelectedProject } from "@/stores/useProjectStore";
import {
  type McpServer,
  fetchMcpServers,
  createMcpServer,
  updateMcpServer,
  deleteMcpServer,
} from "@/api/projectTools";

// ── Server Form ──────────────────────────────────────────────────────────────

interface ServerFormProps {
  initial?: McpServer;
  submitLabel: string;
  isBusy: boolean;
  onSubmit: (data: { name: string; url?: string; command?: string; args?: string[]; env?: Record<string, string> }) => void;
  onCancel: () => void;
}

function ServerForm({ initial, submitLabel, isBusy, onSubmit, onCancel }: ServerFormProps) {
  const [name, setName] = useState(initial?.name ?? "");
  const [transport, setTransport] = useState<"http" | "stdio">(
    initial?.url ? "http" : initial?.command ? "stdio" : "http",
  );
  const [url, setUrl] = useState(initial?.url ?? "");
  const [command, setCommand] = useState(initial?.command ?? "");
  const [args, setArgs] = useState((initial?.args ?? []).join(" "));
  const [envText, setEnvText] = useState(
    Object.entries(initial?.env ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
  );

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const env: Record<string, string> = {};
    for (const line of envText.split("\n")) {
      const eq = line.indexOf("=");
      if (eq > 0) {
        env[line.slice(0, eq).trim()] = line.slice(eq + 1).trim();
      }
    }
    onSubmit({
      name: name.trim(),
      url: transport === "http" ? url.trim() || undefined : undefined,
      command: transport === "stdio" ? command.trim() || undefined : undefined,
      args: transport === "stdio" && args.trim() ? args.trim().split(/\s+/) : undefined,
      env: Object.keys(env).length > 0 ? env : undefined,
    });
  };

  return (
    <form onSubmit={handleSubmit} className="space-y-4">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-foreground">
          {initial ? `Edit "${initial.name}"` : "Add MCP Server"}
        </h3>
        <div className="flex gap-2">
          <Button type="button" variant="outline" size="sm" onClick={onCancel} disabled={isBusy}>
            Cancel
          </Button>
          <Button type="submit" size="sm" disabled={isBusy || !name.trim()}>
            {isBusy ? "Saving..." : submitLabel}
          </Button>
        </div>
      </div>

      <div className="space-y-3">
        <div className="space-y-1.5">
          <Label htmlFor="server-name" className="text-xs text-muted-foreground">Name</Label>
          <Input
            id="server-name"
            autoFocus
            placeholder="e.g. web-search"
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={!!initial}
            required
          />
        </div>

        <div className="space-y-1.5">
          <Label className="text-xs text-muted-foreground">Transport</Label>
          <div className="flex gap-1.5">
            {(["http", "stdio"] as const).map((t) => (
              <button
                key={t}
                type="button"
                onClick={() => setTransport(t)}
                className={`rounded-md border px-2.5 py-1 text-xs transition-colors ${
                  transport === t
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border bg-card text-muted-foreground hover:bg-muted"
                }`}
              >
                {t === "http" ? "HTTP / SSE" : "Stdio"}
              </button>
            ))}
          </div>
        </div>

        {transport === "http" ? (
          <div className="space-y-1.5">
            <Label htmlFor="server-url" className="text-xs text-muted-foreground">URL</Label>
            <Input
              id="server-url"
              placeholder="http://localhost:9000/mcp"
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              className="font-mono text-sm"
            />
          </div>
        ) : (
          <>
            <div className="space-y-1.5">
              <Label htmlFor="server-command" className="text-xs text-muted-foreground">Command</Label>
              <Input
                id="server-command"
                placeholder="npx"
                value={command}
                onChange={(e) => setCommand(e.target.value)}
                className="font-mono text-sm"
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="server-args" className="text-xs text-muted-foreground">Arguments (space-separated)</Label>
              <Input
                id="server-args"
                placeholder="-y @example/mcp-server"
                value={args}
                onChange={(e) => setArgs(e.target.value)}
                className="font-mono text-sm"
              />
            </div>
          </>
        )}

        <div className="space-y-1.5">
          <Label htmlFor="server-env" className="text-xs text-muted-foreground">
            Environment variables (KEY=value, one per line)
          </Label>
          <textarea
            id="server-env"
            placeholder={"API_KEY=${MY_API_KEY}\nDEBUG=true"}
            value={envText}
            onChange={(e) => setEnvText(e.target.value)}
            className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm text-foreground min-h-[60px] resize-none focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            rows={3}
          />
        </div>
      </div>
    </form>
  );
}

// ── Server Card ──────────────────────────────────────────────────────────────

function ServerCard({
  server,
  onEdit,
  onDelete,
  isDeleting,
}: {
  server: McpServer;
  onEdit: () => void;
  onDelete: () => void;
  isDeleting: boolean;
}) {
  const transport = server.url ? "http" : "stdio";
  const endpoint = server.url ?? server.command ?? "—";

  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-2">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-medium">{server.name}</span>
            <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
              {transport}
            </span>
          </div>
          <p className="text-xs text-muted-foreground font-mono mt-0.5 truncate">{endpoint}</p>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Button variant="outline" size="sm" onClick={onEdit}>
            Edit
          </Button>
          <ConfirmButton
            title="Delete MCP server"
            description={`Remove "${server.name}" from mcp.json?`}
            confirmLabel="Delete"
            onConfirm={onDelete}
            size="sm"
            disabled={isDeleting}
          >
            {isDeleting ? "Deleting..." : "Delete"}
          </ConfirmButton>
        </div>
      </div>
      {server.args.length > 0 && (
        <p className="text-xs text-muted-foreground font-mono">
          args: {server.args.join(" ")}
        </p>
      )}
      {Object.keys(server.env).length > 0 && (
        <div className="text-xs text-muted-foreground font-mono">
          {Object.entries(server.env).map(([k, v]) => (
            <p key={k}>{k}={v}</p>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────────────────

export function McpServersManager() {
  const project = useSelectedProject();
  const [servers, setServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [isCreating, setIsCreating] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);
  const [editingName, setEditingName] = useState<string | null>(null);
  const [editBusy, setEditBusy] = useState(false);
  const [deletingName, setDeletingName] = useState<string | null>(null);

  const loadServers = useCallback(async () => {
    if (!project?.id) return;
    setLoading(true);
    setError(null);
    try {
      setServers(await fetchMcpServers(project.id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load MCP servers");
    } finally {
      setLoading(false);
    }
  }, [project?.id]);

  useEffect(() => {
    void loadServers();
  }, [loadServers]);

  if (!project) {
    return (
      <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
        Select a project to manage MCP servers.
      </div>
    );
  }

  const handleCreate = async (data: { name: string; url?: string; command?: string; args?: string[]; env?: Record<string, string> }) => {
    setCreateBusy(true);
    try {
      const server = await createMcpServer({ ...data, project_id: project.id });
      setServers((prev) => [...prev, server].sort((a, b) => a.name.localeCompare(b.name)));
      setIsCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create server");
    } finally {
      setCreateBusy(false);
    }
  };

  const handleUpdate = async (data: { name: string; url?: string; command?: string; args?: string[]; env?: Record<string, string> }) => {
    setEditBusy(true);
    try {
      const updated = await updateMcpServer({ ...data, project_id: project.id });
      setServers((prev) => prev.map((s) => (s.name === updated.name ? updated : s)));
      setEditingName(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update server");
    } finally {
      setEditBusy(false);
    }
  };

  const handleDelete = async (name: string) => {
    setDeletingName(name);
    try {
      await deleteMcpServer(project.id, name);
      setServers((prev) => prev.filter((s) => s.name !== name));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete server");
    } finally {
      setDeletingName(null);
    }
  };

  if (isCreating) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <ServerForm
          submitLabel="Add"
          isBusy={createBusy}
          onSubmit={(data) => void handleCreate(data)}
          onCancel={() => setIsCreating(false)}
        />
      </div>
    );
  }

  const editingServer = editingName ? servers.find((s) => s.name === editingName) : null;
  if (editingServer) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <ServerForm
          initial={editingServer}
          submitLabel="Save"
          isBusy={editBusy}
          onSubmit={(data) => void handleUpdate(data)}
          onCancel={() => setEditingName(null)}
        />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h3 className="text-xl font-bold">MCP Servers</h3>
          <p className="text-sm text-muted-foreground">
            Servers registered in your project's mcp.json. Agents can be assigned these servers as tools.
          </p>
        </div>
        <Button onClick={() => setIsCreating(true)}>Add Server</Button>
      </div>

      {error && <InlineError message={error} onRetry={() => void loadServers()} />}

      {loading ? (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          Loading...
        </div>
      ) : servers.length === 0 ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No MCP servers configured. Add a server to mcp.json to give agents access to external tools.
        </div>
      ) : (
        <div className="space-y-2">
          {servers.map((server) => (
            <ServerCard
              key={server.name}
              server={server}
              onEdit={() => setEditingName(server.name)}
              onDelete={() => void handleDelete(server.name)}
              isDeleting={deletingName === server.name}
            />
          ))}
        </div>
      )}
    </div>
  );
}
