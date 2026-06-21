import { useMemo, useState } from "react";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  File01Icon,
  Folder01Icon,
  FolderOpenIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockShell } from "./BlockShell";
import type { BlockProps } from "./types";
import {
  buildFileTree,
  parseFileTreeBody,
  tallyChanges,
  type FileChange,
  type TreeNode,
} from "./fileTree";

/**
 * FileTree — an interactive VS Code / GitHub-explorer file & change tree.
 *
 * The freeform body text (indented ASCII tree OR one slash-path per line, with
 * optional `(NEW)`/`(MODIFIED)`/`(DELETED)`/`(RENAMED)` markers and trailing
 * notes) is parsed into a flat file list, then folded into a nested folder tree
 * (folders before files, single-child chains compacted). Folders expand/collapse
 * (top level open by default); files carry an A/M/D/R change badge and a muted
 * inline note. A header tallies `+N ~N −N »N`.
 *
 * If parsing yields nothing (weird input), we fall back to the original raw
 * `<pre>` so an existing proposal never blanks or crashes.
 */
export function FileTreeBlock({ attributes, children }: BlockProps) {
  const root = attributes.root;
  const content = typeof children === "string" ? children.trim() : "";

  const files = useMemo(() => parseFileTreeBody(content), [content]);
  const tree = useMemo(() => buildFileTree(files), [files]);
  const counts = useMemo(() => tallyChanges(files), [files]);
  const changeTotal =
    counts.added + counts.modified + counts.deleted + counts.renamed;

  // Top-level folders default open; deeper folders collapse so the tree is
  // scannable. Keyed by full folder path.
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() => {
    const initial: Record<string, boolean> = {};
    const seed = (nodes: TreeNode[], depth: number) => {
      for (const node of nodes) {
        if (node.kind === "folder") {
          initial[node.path] = depth >= 1;
          seed(node.children, depth + 1);
        }
      }
    };
    seed(tree, 0);
    return initial;
  });

  const toggle = (path: string) =>
    setCollapsed((current) => ({ ...current, [path]: !current[path] }));

  // Parsing produced no usable files — fall back to the raw monospace render so
  // an oddly-authored tree is never lost.
  if (files.length === 0) {
    return (
      <BlockShell
        label="Files"
        accent="text-slate-400"
        flush
        meta={
          root ? <span className="truncate font-mono">{root}</span> : null
        }
      >
        <pre className="overflow-x-auto px-4 py-3 font-mono text-xs leading-relaxed text-foreground">
          {content}
        </pre>
      </BlockShell>
    );
  }

  return (
    <BlockShell
      label="Files"
      accent="text-slate-400"
      flush
      meta={
        <span className="flex min-w-0 items-center gap-2 font-mono">
          {root ? <span className="truncate">{root}</span> : null}
          <span className="text-muted-foreground">
            {files.length} {files.length === 1 ? "file" : "files"}
          </span>
          {changeTotal > 0 && (
            <span className="flex items-center gap-1.5">
              {counts.added > 0 && (
                <span className="text-emerald-400">+{counts.added}</span>
              )}
              {counts.modified > 0 && (
                <span className="text-blue-400">~{counts.modified}</span>
              )}
              {counts.deleted > 0 && (
                <span className="text-red-400">−{counts.deleted}</span>
              )}
              {counts.renamed > 0 && (
                <span className="text-violet-400">»{counts.renamed}</span>
              )}
            </span>
          )}
        </span>
      }
    >
      <div className="py-1.5 text-[13px]">
        <TreeRows nodes={tree} depth={0} collapsed={collapsed} toggle={toggle} />
      </div>
    </BlockShell>
  );
}

const INDENT_STEP = 14; // px per nesting level — the explorer guide spacing.

const CHANGE_GLYPH: Record<FileChange, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
};

const CHANGE_LABEL: Record<FileChange, string> = {
  added: "Added",
  modified: "Modified",
  deleted: "Deleted",
  renamed: "Renamed",
};

/** Tinted badge background + saturated text. Dark-only theme. */
const CHANGE_BADGE: Record<FileChange, string> = {
  added: "bg-emerald-500/15 text-emerald-300",
  modified: "bg-blue-500/15 text-blue-300",
  deleted: "bg-red-500/15 text-red-300",
  renamed: "bg-violet-500/15 text-violet-300",
};

/** Accent ink for the file name, echoing its change color. */
const CHANGE_NAME_INK: Record<FileChange, string> = {
  added: "text-emerald-300",
  modified: "text-blue-300",
  deleted: "text-red-300 line-through",
  renamed: "text-violet-300",
};

interface TreeRowsProps {
  nodes: TreeNode[];
  depth: number;
  collapsed: Record<string, boolean>;
  toggle: (path: string) => void;
}

function TreeRows({ nodes, depth, collapsed, toggle }: TreeRowsProps) {
  return (
    <>
      {nodes.map((node) => {
        const indent = depth * INDENT_STEP;
        if (node.kind === "folder") {
          const isCollapsed = collapsed[node.path] ?? false;
          return (
            <div key={`d:${node.path}`}>
              <button
                type="button"
                aria-expanded={!isCollapsed}
                onClick={() => toggle(node.path)}
                style={{ paddingLeft: indent + 12 }}
                className="flex w-full items-center gap-1.5 rounded-md py-1 pr-3 text-left transition-colors hover:bg-muted/50"
              >
                <HugeiconsIcon
                  icon={isCollapsed ? ArrowRight01Icon : ArrowDown01Icon}
                  size={14}
                  className="shrink-0 text-muted-foreground"
                />
                <HugeiconsIcon
                  icon={isCollapsed ? Folder01Icon : FolderOpenIcon}
                  size={15}
                  className="shrink-0 text-sky-400/80"
                />
                <span className="min-w-0 truncate font-medium text-foreground">
                  {node.name}
                </span>
              </button>
              {!isCollapsed && (
                <TreeRows
                  nodes={node.children}
                  depth={depth + 1}
                  collapsed={collapsed}
                  toggle={toggle}
                />
              )}
            </div>
          );
        }

        const change = node.change;
        return (
          <div
            key={`f:${node.path}`}
            style={{ paddingLeft: indent + 12 + 18 }}
            className="flex items-center gap-1.5 py-1 pr-3"
          >
            <HugeiconsIcon
              icon={File01Icon}
              size={15}
              className={cn(
                "shrink-0",
                change === "deleted"
                  ? "text-muted-foreground"
                  : "text-muted-foreground/80",
              )}
            />
            <span
              className={cn(
                "min-w-0 truncate font-medium",
                change ? CHANGE_NAME_INK[change] : "text-foreground",
              )}
            >
              {node.name}
            </span>
            {change && (
              <span
                title={CHANGE_LABEL[change]}
                aria-label={CHANGE_LABEL[change]}
                className={cn(
                  "ml-0.5 flex size-4 shrink-0 items-center justify-center rounded text-[10px] font-bold leading-none",
                  CHANGE_BADGE[change],
                )}
              >
                {CHANGE_GLYPH[change]}
              </span>
            )}
            {node.note && (
              <span className="ml-1 min-w-0 flex-1 truncate text-xs text-muted-foreground">
                {node.note}
              </span>
            )}
          </div>
        );
      })}
    </>
  );
}
