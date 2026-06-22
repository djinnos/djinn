import { useMemo, useState } from "react";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  LockKeyIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockMarkdown, BlockSection } from "./BlockShell";
import { JsonTree } from "./JsonExplorerBlock";
import {
  ACCENT_BADGE,
  httpMethodColor,
  paramLocationColor,
  statusCodeColor,
} from "./blockBadgeColors";
import type { BlockProps } from "./types";
import {
  classifyStatus,
  hasStructuredContent,
  mentionsAuth,
  mentionsDeprecated,
  normalizeMethod,
  parseApiEndpointBody,
  type ApiMethod,
  type ApiParam,
  type ApiResponse,
} from "./apiEndpoint";

/**
 * ApiEndpoint — a Swagger / Stripe-style API reference row.
 *
 * djinn does not serialize structured `request_schema` / `response_schema` JSON
 * props; the endpoint detail is the block's `method` + `path` attributes plus its
 * *children markdown* (a prose description, an optional `## Parameters` table /
 * list, an optional `## Responses` table / list, and fenced JSON example bodies).
 * We parse the children (see `apiEndpoint.ts`) into a structured shape and render:
 *
 *   - a color-coded METHOD pill (per verb) + mono path, with an optional
 *     `Deprecated` badge and a lock icon when the body mentions auth/bearer;
 *   - a markdown description;
 *   - a PARAMETERS section (NAME · IN · TYPE · REQUIRED · DESCRIPTION) with
 *     IN-location pills (path violet / query blue / header amber / body emerald)
 *     and a red `required` marker;
 *   - a RESPONSES section with status-code pills colored by leading digit
 *     (2xx emerald / 4xx amber / 5xx red), each with an optional example body.
 *
 * JSON example bodies render in a styled code surface for now. A dedicated
 * interactive json-explorer block is planned — see the TODO seam in `JsonExample`.
 *
 * If the children have no recognizable params / responses, we fall back to the
 * method/path header + raw `BlockMarkdown` of the children so nothing is lost and
 * the block never crashes.
 */
export function ApiEndpointBlock({ attributes, children }: BlockProps) {
  const method = normalizeMethod(attributes.method);
  const rawMethod = attributes.method?.trim().toUpperCase();
  const path = attributes.path;
  const content = typeof children === "string" ? children : "";

  const parsed = useMemo(() => parseApiEndpointBody(content), [content]);
  const structured = hasStructuredContent(parsed);
  const hasAuth = useMemo(() => mentionsAuth(content), [content]);
  const deprecated = useMemo(() => mentionsDeprecated(content), [content]);

  // Default-open: proposals render full-width and reviewers want detail up front.
  const [open, setOpen] = useState(true);

  const header = (
    <span className="flex min-w-0 items-center gap-2">
      <MethodPill method={method} raw={rawMethod} />
      {path ? (
        <span className="truncate font-mono text-foreground">{path}</span>
      ) : null}
      {deprecated && (
        <span
          className={cn(
            "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide",
            ACCENT_BADGE.amber,
          )}
        >
          Deprecated
        </span>
      )}
      {hasAuth && (
        <HugeiconsIcon
          icon={LockKeyIcon}
          size={13}
          className="shrink-0 text-amber-400/80"
          aria-label="Requires authentication"
        />
      )}
    </span>
  );

  // De-chromed: the colored METHOD + mono path line is the block's own header —
  // it reads as a labelled endpoint in the document, no grey "API ENDPOINT" card
  // around it.
  const headerRow = (
    <div className="flex items-center gap-2 text-sm font-medium">{header}</div>
  );

  // No structured content — keep the method/path header and render the raw
  // children markdown so an oddly-authored endpoint is never blanked.
  if (!structured) {
    return (
      <BlockSection>
        {headerRow}
        <div className="mt-2">
          <BlockMarkdown>{children}</BlockMarkdown>
        </div>
      </BlockSection>
    );
  }

  return (
    <BlockSection>
      {headerRow}
      <div className="mt-1.5 flex flex-col">
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
          className="flex items-center gap-1.5 rounded py-1 text-left text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          <HugeiconsIcon
            icon={open ? ArrowDown01Icon : ArrowRight01Icon}
            size={14}
            className="shrink-0"
          />
          <span className="uppercase tracking-wider">
            {open ? "Hide details" : "Show details"}
          </span>
          <span className="ml-1 flex items-center gap-2 normal-case">
            {parsed.params.length > 0 && (
              <span>
                {parsed.params.length}{" "}
                {parsed.params.length === 1 ? "param" : "params"}
              </span>
            )}
            {parsed.responses.length > 0 && (
              <span>
                {parsed.responses.length}{" "}
                {parsed.responses.length === 1 ? "response" : "responses"}
              </span>
            )}
          </span>
        </button>

        {open && (
          <div className="flex flex-col gap-4 pt-2">
            {parsed.description && (
              <BlockMarkdown>{parsed.description}</BlockMarkdown>
            )}

            {parsed.params.length > 0 && (
              <ParamsSection params={parsed.params} />
            )}

            {parsed.requestExample && (
              <section>
                <SectionLabel>Request body</SectionLabel>
                <JsonExample code={parsed.requestExample} />
              </section>
            )}

            {parsed.responses.length > 0 && (
              <ResponsesSection responses={parsed.responses} />
            )}
          </div>
        )}
      </div>
    </BlockSection>
  );
}

/* ── Method pill ────────────────────────────────────────────────────────────── */

function MethodPill({
  method,
  raw,
}: {
  method: ApiMethod | undefined;
  raw: string | undefined;
}) {
  const label = method ?? raw;
  if (!label) return null;
  return (
    <span
      className={cn(
        "shrink-0 rounded px-1.5 py-0.5 font-mono text-[11px] font-bold tracking-wide",
        httpMethodColor(method),
      )}
    >
      {label}
    </span>
  );
}

/* ── Parameters section ─────────────────────────────────────────────────────── */

function ParamsSection({ params }: { params: ApiParam[] }) {
  return (
    <section>
      <SectionLabel>Parameters</SectionLabel>
      <div className="overflow-hidden rounded-lg border bg-card/50">
        <div className="divide-y divide-border/60">
          {params.map((param, index) => (
            <ParamRow key={`${param.name}-${index}`} param={param} />
          ))}
        </div>
      </div>
    </section>
  );
}

function ParamRow({ param }: { param: ApiParam }) {
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-2 text-[13px]">
      <span className="font-mono font-medium text-foreground">
        {param.name}
      </span>
      {param.in && (
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
            paramLocationColor(param.in),
          )}
        >
          {param.in}
        </span>
      )}
      {param.type && (
        <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {param.type}
        </span>
      )}
      {param.required === true && (
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
            ACCENT_BADGE.red,
          )}
        >
          required
        </span>
      )}
      {param.required === false && (
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
            ACCENT_BADGE.slate,
          )}
        >
          optional
        </span>
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

function ResponsesSection({ responses }: { responses: ApiResponse[] }) {
  return (
    <section>
      <SectionLabel>Responses</SectionLabel>
      <div className="flex flex-col gap-2">
        {responses.map((response, index) => (
          <ResponseRow key={`${response.status}-${index}`} response={response} />
        ))}
      </div>
    </section>
  );
}

function ResponseRow({ response }: { response: ApiResponse }) {
  const cls = classifyStatus(response.status);
  return (
    <div className="overflow-hidden rounded-lg border bg-card/50">
      <div className="flex flex-wrap items-center gap-2 px-3 py-2 text-[13px]">
        <span
          className={cn(
            "rounded px-1.5 py-0.5 font-mono text-[11px] font-bold leading-none",
            statusCodeColor(cls),
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
          <JsonExample code={response.example} />
        </div>
      )}
    </div>
  );
}

/* ── Shared bits ────────────────────────────────────────────────────────────── */

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="mb-1.5 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
      {children}
    </div>
  );
}

/**
 * A JSON / example body rendered with the shared interactive `json-explorer`
 * tree: parseable JSON becomes a collapsible, type-colored tree (Postman-style)
 * and any non-JSON / unparseable body gracefully falls back to a styled `<pre>`
 * — both handled inside `JsonTree`, so an example is never lost or misrendered.
 */
function JsonExample({ code }: { code: string }) {
  return <JsonTree code={code} label="Example" />;
}
