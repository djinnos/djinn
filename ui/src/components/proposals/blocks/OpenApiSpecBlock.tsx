import { useMemo, useState } from "react";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  LockKeyIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import { JsonTree } from "./JsonExplorerBlock";
import { Badge } from "./blockBadges";
import {
  httpMethodColor,
  paramLocationColor,
  statusCodeColor,
} from "./blockBadgeColors";
import { classifyStatus, normalizeMethod } from "./apiEndpoint";
import {
  isKnownParamLocation,
  parseOpenApiSpec,
  type OpenApiOperation,
  type OpenApiParam,
  type OpenApiResponse,
  type OpenApiTagGroup,
} from "./openApiSpec";
import type { BlockProps } from "./types";

/**
 * OpenApiSpec — a Redoc / Swagger-UI-style API reference rendered from a whole
 * OpenAPI 3 / Swagger 2 document.
 *
 * djinn does NOT serialize a structured prop; the block's payload is its
 * *children string* — the complete OpenAPI document (JSON). We parse it (see
 * `openApiSpec.ts`): internal `$ref`s are resolved (cycle-guarded), operations
 * are grouped by tag (untagged → a `default` group), and each operation gets a
 * params table, request body, and per-status responses with example bodies
 * derived from the schemas. The renderer reuses the api-endpoint house style:
 * method pills, param-location pills, status-code pills, and the shared
 * interactive `JsonTree` for every example body.
 *
 * Graceful + read-only: if the children are not a parseable OpenAPI document the
 * renderer falls back to the shared `JsonTree` (which itself falls back to a
 * styled `<pre>`) plus the parse error, so an oddly-authored block never blanks
 * or crashes. YAML is not supported in v1 — a YAML body yields an actionable
 * fallback message (see `parseOpenApiSpec`).
 */
export function OpenApiSpecBlock({ attributes, children }: BlockProps) {
  const content = typeof children === "string" ? children : "";
  const title = attributes.title?.trim() || undefined;

  const parsed = useMemo(() => parseOpenApiSpec(content), [content]);

  if (!parsed.ok) {
    return (
      <BlockShell
        label="OpenAPI"
        accent="text-green-400"
        flush
        meta={
          title ? <span className="truncate font-mono">{title}</span> : null
        }
      >
        <div className="space-y-2 p-4">
          <p className="text-[11px] text-amber-300">
            Could not parse OpenAPI spec: {parsed.error}
          </p>
          <JsonTree code={content} label="OpenAPI" />
        </div>
      </BlockShell>
    );
  }

  const spec = parsed.spec;
  const meta = (
    <span className="flex min-w-0 items-center gap-2">
      {title ? (
        <span className="truncate font-mono text-foreground">{title}</span>
      ) : (
        <span className="truncate font-mono text-foreground">
          {spec.title || "API reference"}
        </span>
      )}
      {spec.version && (
        <span className="shrink-0 rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          v{spec.version}
        </span>
      )}
      <Badge accent="slate" className="shrink-0 normal-case">
        {spec.format}
      </Badge>
    </span>
  );

  return (
    <BlockShell label="OpenAPI" accent="text-green-400" flush meta={meta}>
      <div className="flex flex-col gap-3 p-4">
        {(spec.description?.trim() || spec.servers.length > 0) && (
          <div className="rounded-lg border bg-card/50 px-3 py-2.5">
            {spec.servers.length > 0 && (
              <div className="mb-2 flex flex-wrap items-center gap-1.5">
                <span className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                  Servers
                </span>
                {spec.servers.map((server) => (
                  <span
                    key={server}
                    className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground"
                  >
                    {server}
                  </span>
                ))}
              </div>
            )}
            {spec.description?.trim() && (
              <BlockMarkdown>{spec.description}</BlockMarkdown>
            )}
            <p className="mt-1 text-[11px] text-muted-foreground">
              {spec.operationCount}{" "}
              {spec.operationCount === 1 ? "operation" : "operations"} across{" "}
              {spec.groups.length}{" "}
              {spec.groups.length === 1 ? "tag" : "tags"}
            </p>
          </div>
        )}

        {spec.groups.length === 0 ? (
          <div className="rounded-lg border bg-card/50 px-3 py-6 text-center text-sm text-muted-foreground">
            No operations found in this spec.
          </div>
        ) : (
          spec.groups.map((group, index) => (
            <TagGroup key={group.tag} group={group} defaultOpen={index === 0} />
          ))
        )}
      </div>
    </BlockShell>
  );
}

/* ── Tag group (collapsible) ────────────────────────────────────────────────── */

function TagGroup({
  group,
  defaultOpen,
}: {
  group: OpenApiTagGroup;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="overflow-hidden rounded-lg border bg-card/50">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition-colors hover:bg-muted/40"
      >
        <HugeiconsIcon
          icon={open ? ArrowDown01Icon : ArrowRight01Icon}
          size={14}
          className="shrink-0 text-muted-foreground"
        />
        <span className="font-semibold text-foreground">{group.tag}</span>
        <span className="rounded-full bg-muted/60 px-2 py-0.5 text-[11px] font-medium text-muted-foreground">
          {group.operations.length}
        </span>
        {group.description && (
          <span className="ml-1 min-w-0 flex-1 truncate text-xs text-muted-foreground">
            {group.description}
          </span>
        )}
      </button>
      {open && (
        <div className="divide-y divide-border/60 border-t">
          {group.operations.map((operation) => (
            <OperationRow key={operation.id} operation={operation} />
          ))}
        </div>
      )}
    </div>
  );
}

/* ── Operation row (collapsible) ────────────────────────────────────────────── */

function OperationRow({ operation }: { operation: OpenApiOperation }) {
  const [open, setOpen] = useState(false);
  const method = normalizeMethod(operation.method);

  const hasBody =
    Boolean(operation.description?.trim()) ||
    operation.params.length > 0 ||
    Boolean(operation.requestExample || operation.requestContentType) ||
    operation.responses.length > 0;

  return (
    <div className="overflow-hidden">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition-colors hover:bg-muted/40"
      >
        <HugeiconsIcon
          icon={open ? ArrowDown01Icon : ArrowRight01Icon}
          size={13}
          className="shrink-0 text-muted-foreground"
        />
        <span
          className={cn(
            "shrink-0 rounded px-1.5 py-0.5 font-mono text-[11px] font-bold tracking-wide",
            httpMethodColor(method),
          )}
        >
          {operation.method}
        </span>
        <span
          className={cn(
            "min-w-0 truncate font-mono text-[13px] font-medium text-foreground",
            operation.deprecated && "text-muted-foreground line-through",
          )}
        >
          {operation.path}
        </span>
        {operation.summary && (
          <span className="ml-1 min-w-0 flex-1 truncate text-xs text-muted-foreground">
            {operation.summary}
          </span>
        )}
        {operation.deprecated && (
          <Badge accent="amber" className="shrink-0">
            Deprecated
          </Badge>
        )}
        {operation.secured && (
          <HugeiconsIcon
            icon={LockKeyIcon}
            size={13}
            className="shrink-0 text-amber-400/80"
            aria-label="Requires authentication"
          />
        )}
      </button>

      {open && hasBody && (
        <div className="flex flex-col gap-4 bg-muted/20 px-3 pb-4 pt-3">
          {operation.description?.trim() && (
            <BlockMarkdown>{operation.description}</BlockMarkdown>
          )}

          {operation.params.length > 0 && (
            <ParamsSection params={operation.params} />
          )}

          {(operation.requestExample || operation.requestContentType) && (
            <section>
              <div className="mb-1.5 flex items-center gap-2">
                <SectionLabel>Request body</SectionLabel>
                {operation.requestContentType && (
                  <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
                    {operation.requestContentType}
                  </span>
                )}
              </div>
              {operation.requestExample && (
                <JsonTree code={operation.requestExample} label="Example" />
              )}
            </section>
          )}

          {operation.responses.length > 0 && (
            <ResponsesSection responses={operation.responses} />
          )}
        </div>
      )}
    </div>
  );
}

/* ── Parameters section ─────────────────────────────────────────────────────── */

function ParamsSection({ params }: { params: OpenApiParam[] }) {
  return (
    <section>
      <SectionLabel>Parameters</SectionLabel>
      <div className="mt-1.5 overflow-hidden rounded-lg border bg-card/50">
        <div className="divide-y divide-border/60">
          {params.map((param, index) => (
            <ParamRow key={`${param.name}-${param.in}-${index}`} param={param} />
          ))}
        </div>
      </div>
    </section>
  );
}

function ParamRow({ param }: { param: OpenApiParam }) {
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-2 text-[13px]">
      <span className="font-mono font-medium text-foreground">
        {param.name}
      </span>
      {param.in &&
        (isKnownParamLocation(param.in) ? (
          <span
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
              paramLocationColor(param.in),
            )}
          >
            {param.in}
          </span>
        ) : (
          <Badge accent="slate">{param.in}</Badge>
        ))}
      {param.type && (
        <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {param.type}
        </span>
      )}
      {param.required === true ? (
        <Badge accent="red">required</Badge>
      ) : (
        <Badge accent="slate">optional</Badge>
      )}
      {param.description && (
        <span className="w-full text-xs text-muted-foreground">
          {param.description}
        </span>
      )}
    </div>
  );
}

/* ── Responses section ──────────────────────────────────────────────────────── */

function ResponsesSection({ responses }: { responses: OpenApiResponse[] }) {
  return (
    <section>
      <SectionLabel>Responses</SectionLabel>
      <div className="mt-1.5 flex flex-col gap-2">
        {responses.map((response, index) => (
          <ResponseRow key={`${response.status}-${index}`} response={response} />
        ))}
      </div>
    </section>
  );
}

function ResponseRow({ response }: { response: OpenApiResponse }) {
  return (
    <div className="overflow-hidden rounded-lg border bg-card/50">
      <div className="flex flex-wrap items-center gap-2 px-3 py-2 text-[13px]">
        <span
          className={cn(
            "rounded px-1.5 py-0.5 font-mono text-[11px] font-bold leading-none",
            statusCodeColor(classifyStatus(response.status)),
          )}
        >
          {response.status}
        </span>
        {response.description && (
          <span className="min-w-0 text-foreground">{response.description}</span>
        )}
      </div>
      {response.example && (
        <div className="border-t px-3 py-2">
          <JsonTree code={response.example} label="Example" />
        </div>
      )}
    </div>
  );
}

/* ── Shared bits ────────────────────────────────────────────────────────────── */

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
      {children}
    </div>
  );
}
