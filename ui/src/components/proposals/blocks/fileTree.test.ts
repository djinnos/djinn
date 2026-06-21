import { describe, expect, it } from "vitest";

import {
  buildFileTree,
  parseFileTreeBody,
  tallyChanges,
  type TreeNode,
} from "./fileTree";

/** Collapse a derived tree into a `kind:path` line list for compact assertions. */
function flatten(nodes: TreeNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    out.push(`${node.kind === "folder" ? "D" : "F"}:${node.path}`);
    if (node.kind === "folder") flatten(node.children, out);
  }
  return out;
}

describe("parseFileTreeBody", () => {
  it("returns [] for empty / whitespace input", () => {
    expect(parseFileTreeBody("")).toEqual([]);
    expect(parseFileTreeBody("   \n  \n")).toEqual([]);
  });

  it("parses one slash-path per line", () => {
    const files = parseFileTreeBody(
      "src/main.rs\nsrc/routes/git.ts\nCargo.toml",
    );
    expect(files.map((f) => f.path)).toEqual([
      "src/main.rs",
      "src/routes/git.ts",
      "Cargo.toml",
    ]);
    expect(files.every((f) => f.change === undefined)).toBe(true);
  });

  it("reconstructs paths from an indented ASCII tree", () => {
    const body = [
      "src/",
      "  main.rs",
      "  routes/",
      "    git.ts",
      "    auth.ts",
      "Cargo.toml",
    ].join("\n");
    const files = parseFileTreeBody(body);
    expect(files.map((f) => f.path)).toEqual([
      "src/main.rs",
      "src/routes/git.ts",
      "src/routes/auth.ts",
      "Cargo.toml",
    ]);
  });

  it("treats a row whose next row is more deeply indented as a folder even without a trailing slash", () => {
    const body = ["src", "  main.rs", "  lib", "    util.rs"].join("\n");
    const files = parseFileTreeBody(body);
    expect(files.map((f) => f.path)).toEqual([
      "src/main.rs",
      "src/lib/util.rs",
    ]);
  });

  it("parses inline (NEW)/(MODIFIED)/(DELETED)/(RENAMED) status markers", () => {
    const files = parseFileTreeBody(
      [
        "src/added.ts (NEW)",
        "src/changed.ts (MODIFIED)",
        "src/gone.ts (DELETED)",
        "src/moved.ts (RENAMED)",
      ].join("\n"),
    );
    expect(files.map((f) => f.change)).toEqual([
      "added",
      "modified",
      "deleted",
      "renamed",
    ]);
  });

  it("accepts common status synonyms case-insensitively", () => {
    const files = parseFileTreeBody(
      ["a.ts (added)", "b.ts (Removed)", "c.ts (updated)", "d.ts (moved)"].join(
        "\n",
      ),
    );
    expect(files.map((f) => f.change)).toEqual([
      "added",
      "deleted",
      "modified",
      "renamed",
    ]);
  });

  it("extracts a trailing note as a caption, keeping it distinct from a status", () => {
    const files = parseFileTreeBody(
      "Cargo.toml (add [workspace.lints])\nsrc/x.rs (MODIFIED) wire the new route",
    );
    expect(files[0]).toMatchObject({
      path: "Cargo.toml",
      note: "add [workspace.lints]",
    });
    expect(files[0]!.change).toBeUndefined();
    expect(files[1]).toMatchObject({
      path: "src/x.rs",
      change: "modified",
      note: "wire the new route",
    });
  });

  it("treats an em/en-dash or colon as a note separator", () => {
    const files = parseFileTreeBody(
      "src/a.ts — does the thing\nsrc/b.ts: helper",
    );
    expect(files[0]).toMatchObject({ path: "src/a.ts", note: "does the thing" });
    expect(files[1]).toMatchObject({ path: "src/b.ts", note: "helper" });
  });

  it("attaches status/note to a bare folder row that carries them", () => {
    const files = parseFileTreeBody("src/legacy/ (DELETED) drop the old module");
    expect(files).toHaveLength(1);
    expect(files[0]).toMatchObject({
      path: "src/legacy",
      change: "deleted",
      note: "drop the old module",
    });
  });

  it("ignores bare folder header rows with no change/note", () => {
    const files = parseFileTreeBody("src/\nsrc/main.rs");
    expect(files.map((f) => f.path)).toEqual(["src/main.rs"]);
  });

  it("strips ASCII tree-drawing glyphs", () => {
    const body = ["src", "  ├─ main.rs", "  └─ lib.rs"].join("\n");
    const files = parseFileTreeBody(body);
    expect(files.map((f) => f.path)).toEqual(["src/main.rs", "src/lib.rs"]);
  });

  it("dedupes repeated paths, keeping the first", () => {
    const files = parseFileTreeBody("a.ts (NEW)\na.ts (MODIFIED)");
    expect(files).toHaveLength(1);
    expect(files[0]!.change).toBe("added");
  });

  it("returns [] when every line is pure punctuation (no path-ish token)", () => {
    expect(parseFileTreeBody("---\n===\n***")).toEqual([]);
  });
});

describe("buildFileTree", () => {
  it("derives a nested folder tree, folders before files", () => {
    const files = parseFileTreeBody(
      ["src/routes/git.ts", "src/main.rs", "Cargo.toml"].join("\n"),
    );
    const tree = buildFileTree(files);
    // folders before files at each level: src/ then Cargo.toml at the root.
    expect(flatten(tree)).toEqual([
      "D:src",
      "D:src/routes",
      "F:src/routes/git.ts",
      "F:src/main.rs",
      "F:Cargo.toml",
    ]);
  });

  it("compacts single-child folder chains into one row", () => {
    const files = parseFileTreeBody("a/b/c/deep.ts");
    const tree = buildFileTree(files);
    expect(tree).toHaveLength(1);
    expect(tree[0]).toMatchObject({ kind: "folder", name: "a/b/c" });
    expect((tree[0] as { children: TreeNode[] }).children[0]).toMatchObject({
      kind: "file",
      path: "a/b/c/deep.ts",
    });
  });

  it("does NOT compact a folder that has more than one child", () => {
    const files = parseFileTreeBody("a/b/one.ts\na/c/two.ts");
    const tree = buildFileTree(files);
    // `a` has two folder children (b, c) so it must not be compacted away.
    expect(tree[0]).toMatchObject({ kind: "folder", name: "a" });
    expect((tree[0] as { children: TreeNode[] }).children).toHaveLength(2);
  });

  it("carries change + note onto file leaves", () => {
    const files = parseFileTreeBody("src/x.ts (NEW) added the route");
    const tree = buildFileTree(files);
    const folder = tree[0] as { children: TreeNode[] };
    expect(folder.children[0]).toMatchObject({
      kind: "file",
      change: "added",
      note: "added the route",
    });
  });
});

describe("tallyChanges", () => {
  it("counts each change kind", () => {
    const files = parseFileTreeBody(
      [
        "a.ts (NEW)",
        "b.ts (NEW)",
        "c.ts (MODIFIED)",
        "d.ts (DELETED)",
        "e.ts (RENAMED)",
        "f.ts",
      ].join("\n"),
    );
    expect(tallyChanges(files)).toEqual({
      added: 2,
      modified: 1,
      deleted: 1,
      renamed: 1,
    });
  });
});
