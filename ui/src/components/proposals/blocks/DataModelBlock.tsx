import { useMemo, useState } from "react";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  ArrowRight02Icon,
  Database01Icon,
  Key01Icon,
  Link01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import type { BlockProps } from "./types";
import {
  parseDataModelBlock,
  type DataModelEntity,
  type DataModelField,
  type DataModelRelation,
} from "./dataModel";

/**
 * DataModel — an interactive entity-relationship (ERD) renderer.
 *
 * djinn does not serialize a structured `fields={…}` JSON prop; the data-model
 * content is the block's *children markdown* (a `| field | type | notes |`
 * table, a bulleted list, or several `### Entity` sections). We parse that into
 * entities + fields (heuristically detecting PK / FK / nullable / default / enum
 * hints) and infer the relations between entities from their FK fields.
 *
 * Each entity renders as a collapsible card (database icon + name + `N fields`
 * pill); field rows carry PK/FK icons, a type pill, right-aligned badges, and
 * inline enum values. A RELATIONS section lists the inferred FK relationships
 * with cardinality pills, tagging `(unresolved)` targets in amber.
 *
 * If parsing yields no recognisable fields, we fall back to the original
 * `BlockMarkdown` render so an oddly-authored model is never lost or crashes.
 */
export function DataModelBlock({ attributes, children }: BlockProps) {
  const name = attributes.name;
  const content = typeof children === "string" ? children : "";

  const { entities, relations } = useMemo(
    () => parseDataModelBlock(content, name),
    [content, name],
  );

  // Default-open the first entity, or all of them when the model is small (≤2),
  // so the block is useful at a glance without a click.
  const [open, setOpen] = useState<Record<string, boolean>>(() => {
    const initial: Record<string, boolean> = {};
    const openAll = entities.length <= 2;
    entities.forEach((entity, index) => {
      initial[entity.name] = openAll || index === 0;
    });
    return initial;
  });

  const toggle = (key: string) =>
    setOpen((current) => ({ ...current, [key]: !current[key] }));

  // Parsing produced nothing usable — fall back to the raw markdown render so a
  // hand-authored model that doesn't match our shapes is never blanked.
  if (entities.length === 0) {
    return (
      <BlockShell
        label="Data Model"
        accent="text-blue-400"
        meta={name ? <span className="font-mono">{name}</span> : null}
      >
        <BlockMarkdown>{children}</BlockMarkdown>
      </BlockShell>
    );
  }

  const totalFields = entities.reduce((sum, e) => sum + e.fields.length, 0);

  return (
    <BlockShell
      label="Data Model"
      accent="text-blue-400"
      flush
      meta={
        <span className="flex min-w-0 items-center gap-2 font-mono">
          {name ? <span className="truncate">{name}</span> : null}
          <span className="text-muted-foreground">
            {entities.length} {entities.length === 1 ? "entity" : "entities"}
          </span>
          <span className="text-muted-foreground">
            {totalFields} {totalFields === 1 ? "field" : "fields"}
          </span>
        </span>
      }
    >
      <div className="flex flex-col gap-2 p-3">
        {entities.map((entity, index) => (
          <EntityCard
            key={`${entity.name}-${index}`}
            entity={entity}
            isOpen={open[entity.name] ?? false}
            onToggle={() => toggle(entity.name)}
          />
        ))}

        {relations.length > 0 && <RelationsSection relations={relations} />}
      </div>
    </BlockShell>
  );
}

/* ── Entity card ────────────────────────────────────────────────────────────── */

interface EntityCardProps {
  entity: DataModelEntity;
  isOpen: boolean;
  onToggle: () => void;
}

function EntityCard({ entity, isOpen, onToggle }: EntityCardProps) {
  return (
    <div className="overflow-hidden rounded-lg border bg-card">
      <button
        type="button"
        aria-expanded={isOpen}
        onClick={onToggle}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition-colors hover:bg-muted/50"
      >
        <HugeiconsIcon
          icon={isOpen ? ArrowDown01Icon : ArrowRight01Icon}
          size={14}
          className="shrink-0 text-muted-foreground"
        />
        <HugeiconsIcon
          icon={Database01Icon}
          size={15}
          className="shrink-0 text-blue-400"
        />
        <span className="min-w-0 truncate font-mono text-sm font-semibold text-foreground">
          {entity.name}
        </span>
        <span className="ml-auto shrink-0 rounded-full bg-muted/60 px-2 py-0.5 text-[11px] font-medium text-muted-foreground">
          {entity.fields.length}{" "}
          {entity.fields.length === 1 ? "field" : "fields"}
        </span>
      </button>

      {isOpen && (
        <div className="border-t">
          {entity.note && (
            <p className="px-3 pt-2 text-xs italic text-muted-foreground">
              {entity.note}
            </p>
          )}
          <div className="divide-y divide-border/60">
            {entity.fields.map((field, index) => (
              <FieldRow key={`${field.name}-${index}`} field={field} />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/* ── Field row ──────────────────────────────────────────────────────────────── */

function FieldRow({ field }: { field: DataModelField }) {
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-1.5">
      {/* Leading PK/FK icon (or a spacer to keep names aligned). */}
      <span className="flex w-4 shrink-0 justify-center">
        {field.pk ? (
          <HugeiconsIcon
            icon={Key01Icon}
            size={14}
            className="text-amber-400"
            aria-label="Primary key"
          />
        ) : field.fk ? (
          <HugeiconsIcon
            icon={Link01Icon}
            size={14}
            className="text-blue-400"
            aria-label="Foreign key"
          />
        ) : null}
      </span>

      <span
        className={cn(
          "min-w-0 font-mono text-[13px]",
          field.pk ? "font-semibold text-foreground" : "text-foreground",
        )}
      >
        {field.name}
      </span>

      {field.type && (
        <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {field.type}
        </span>
      )}

      {/* Enum values inline. */}
      {field.enumValues && field.enumValues.length > 0 && (
        <span className="flex flex-wrap items-center gap-1">
          {field.enumValues.map((value) => (
            <span
              key={value}
              className="rounded bg-violet-500/15 px-1.5 py-0.5 font-mono text-[10px] text-violet-300"
            >
              {value}
            </span>
          ))}
        </span>
      )}

      {/* Right-aligned badges. */}
      <span className="ml-auto flex flex-wrap items-center justify-end gap-1">
        {field.pk && (
          <Badge className="bg-amber-500/15 text-amber-300">PK</Badge>
        )}
        {field.fk && (
          <Badge className="bg-blue-500/15 text-blue-300">
            FK
            <span className="ml-1 font-mono font-normal opacity-90">
              {field.fk.table}
              {field.fk.column ? `.${field.fk.column}` : ""}
            </span>
          </Badge>
        )}
        {field.nullable && (
          <Badge className="bg-slate-500/15 text-slate-300">nullable</Badge>
        )}
        {field.default !== undefined && (
          <Badge className="bg-emerald-500/15 text-emerald-300 normal-case">
            = {field.default}
          </Badge>
        )}
      </span>

      {field.note && (
        <span className="w-full pl-6 text-xs text-muted-foreground">
          {field.note}
        </span>
      )}
    </div>
  );
}

function Badge({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
        className,
      )}
    >
      {children}
    </span>
  );
}

/* ── Relations section ──────────────────────────────────────────────────────── */

const KIND_LABEL: Record<DataModelRelation["kind"], string> = {
  "1-1": "1:1",
  "1-n": "1:n",
  "n-n": "n:n",
};

function RelationsSection({ relations }: { relations: DataModelRelation[] }) {
  return (
    <div className="rounded-lg border bg-card/50">
      <div className="border-b px-3 py-1.5 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
        Relations
      </div>
      <div className="flex flex-col gap-1 p-2">
        {relations.map((relation, index) => (
          <div
            key={`${relation.from}-${relation.to}-${relation.label}-${index}`}
            className="flex flex-wrap items-center gap-2 px-1 py-0.5 text-[13px]"
          >
            <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[10px] font-semibold text-muted-foreground">
              {KIND_LABEL[relation.kind]}
            </span>
            <span className="font-mono font-medium text-foreground">
              {relation.from}
            </span>
            <HugeiconsIcon
              icon={ArrowRight02Icon}
              size={14}
              className="text-muted-foreground"
            />
            <span
              className={cn(
                "font-mono font-medium",
                relation.unresolved ? "text-amber-300" : "text-foreground",
              )}
            >
              {relation.to}
            </span>
            <span className="font-mono text-xs text-muted-foreground">
              ({relation.label})
            </span>
            {relation.unresolved && (
              <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-300">
                unresolved
              </span>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
