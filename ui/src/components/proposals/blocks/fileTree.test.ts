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

  it("reads the DECLARED leading status token (+ ~ - >) for each kind", () => {
    const files = parseFileTreeBody(
      [
        "+ src/added.ts",
        "~ src/changed.ts",
        "- src/gone.ts",
        "> src/moved.ts",
      ].join("\n"),
    );
    expect(files.map((f) => f.path)).toEqual([
      "src/added.ts",
      "src/changed.ts",
      "src/gone.ts",
      "src/moved.ts",
    ]);
    expect(files.map((f) => f.change)).toEqual([
      "added",
      "modified",
      "deleted",
      "renamed",
    ]);
  });

  it("accepts the U+2212 minus sign as a removed token", () => {
    const files = parseFileTreeBody("− src/gone.ts");
    expect(files[0]).toMatchObject({ path: "src/gone.ts", change: "deleted" });
  });

  it("leaves a row with no token as unchanged", () => {
    const files = parseFileTreeBody("src/stable.ts\nsrc/other.ts");
    expect(files.every((f) => f.change === undefined)).toBe(true);
  });

  it("does NOT infer status from English words like new/modified/deleted", () => {
    // Words in a trailing note are notes, never status — only the token decides.
    const files = parseFileTreeBody(
      [
        "src/a.ts (NEW)",
        "src/b.ts (MODIFIED)",
        "src/c.ts — newly added",
      ].join("\n"),
    );
    expect(files.every((f) => f.change === undefined)).toBe(true);
    expect(files[0]).toMatchObject({ path: "src/a.ts", note: "NEW" });
    expect(files[1]).toMatchObject({ path: "src/b.ts", note: "MODIFIED" });
    expect(files[2]).toMatchObject({ path: "src/c.ts", note: "newly added" });
  });

  it("treats a status token as status, NOT as a bullet glyph (collision)", () => {
    // `-`/`+` followed by whitespace are declared tokens, not list bullets.
    const files = parseFileTreeBody("- src/removed.ts\n+ src/created.ts");
    expect(files.map((f) => f.change)).toEqual(["deleted", "added"]);
    expect(files.map((f) => f.path)).toEqual([
      "src/removed.ts",
      "src/created.ts",
    ]);
  });

  it("still treats `*` as a plain bullet (unchanged file)", () => {
    const files = parseFileTreeBody("* src/keep.ts");
    expect(files[0]).toMatchObject({ path: "src/keep.ts" });
    expect(files[0]!.change).toBeUndefined();
  });

  it("requires whitespace after the token: `-rf`/`~` paths are not tokens", () => {
    const files = parseFileTreeBody("-rf.txt\n~cache.tmp");
    expect(files.map((f) => f.path)).toEqual(["-rf.txt", "~cache.tmp"]);
    expect(files.every((f) => f.change === undefined)).toBe(true);
  });

  it("honors declared tokens inside an indented tree", () => {
    const body = [
      "src/",
      "  + added.rs",
      "  ~ changed.rs",
      "  routes/",
      "    - gone.ts",
    ].join("\n");
    const files = parseFileTreeBody(body);
    expect(files).toEqual([
      { path: "src/added.rs", name: "added.rs", change: "added", note: undefined },
      {
        path: "src/changed.rs",
        name: "changed.rs",
        change: "modified",
        note: undefined,
      },
      {
        path: "src/routes/gone.ts",
        name: "gone.ts",
        change: "deleted",
        note: undefined,
      },
    ]);
  });

  it("extracts a trailing note as a caption alongside a declared token", () => {
    const files = parseFileTreeBody(
      "Cargo.toml — add [workspace.lints]\n~ src/x.rs wire the new route",
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

  it("attaches a declared token + note to a bare folder row", () => {
    const files = parseFileTreeBody("- src/legacy/ drop the old module");
    expect(files).toHaveLength(1);
    expect(files[0]).toMatchObject({
      path: "src/legacy",
      change: "deleted",
      note: "drop the old module",
    });
  });

  it("ignores bare folder header rows with no token/note", () => {
    const files = parseFileTreeBody("src/\nsrc/main.rs");
    expect(files.map((f) => f.path)).toEqual(["src/main.rs"]);
  });

  it("strips ASCII tree-drawing glyphs", () => {
    const body = ["src", "  ├─ main.rs", "  └─ lib.rs"].join("\n");
    const files = parseFileTreeBody(body);
    expect(files.map((f) => f.path)).toEqual(["src/main.rs", "src/lib.rs"]);
  });

  it("honors a declared token after a tree glyph", () => {
    const body = ["src", "  ├─ ~ main.rs", "  └─ + lib.rs"].join("\n");
    const files = parseFileTreeBody(body);
    expect(files).toEqual([
      { path: "src/main.rs", name: "main.rs", change: "modified", note: undefined },
      { path: "src/lib.rs", name: "lib.rs", change: "added", note: undefined },
    ]);
  });

  it("dedupes repeated paths, keeping the first", () => {
    const files = parseFileTreeBody("+ a.ts\n~ a.ts");
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
    const files = parseFileTreeBody("+ src/x.ts added the route");
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
  it("counts each declared change kind", () => {
    const files = parseFileTreeBody(
      [
        "+ a.ts",
        "+ b.ts",
        "~ c.ts",
        "- d.ts",
        "> e.ts",
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
