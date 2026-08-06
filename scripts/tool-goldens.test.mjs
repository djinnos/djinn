// Contract tests for the MCP tool-schema golden mechanism.
//
// Run: node --test scripts/tool-goldens.test.mjs
//
// These assert structured values — manifest shape, classification of real
// files, derivation output bytes — never the wording of a log line.

import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  classifyCandidates,
  discoverCandidates,
  entryPatterns,
  instaSnapshotBody,
  loadManifest,
  matchesGlob,
  producerOutputs,
  producersRunningDownstreamGuards,
  repoRoot,
  staleArtifactPatterns,
  validateManifest,
} from "./lib/tool-goldens.mjs";
import { evaluate, findSilentGuards, goldenPaths } from "./check-tool-goldens.mjs";
import { buildPlan, resolvePnpmCommand, withPnpmOnPath } from "./regenerate-tool-goldens.mjs";

const manifest = loadManifest();

// ── manifest shape ─────────────────────────────────────────────────────────

test("the committed manifest is structurally valid", () => {
  assert.deepEqual(validateManifest(manifest), []);
});

test("every producer writes at least one declared artifact", () => {
  const orphans = manifest.producers
    .filter((producer) => producerOutputs(manifest, producer.id).length === 0)
    .map((producer) => producer.id);
  assert.deepEqual(orphans, []);
});

test("a producer may only consume output an earlier producer already wrote", () => {
  // Reordering the array so a consumer runs before its input is exactly the
  // mistake that would make `make tool-goldens` need two passes.
  const reordered = {
    ...manifest,
    producers: [...manifest.producers].reverse(),
  };
  const problems = validateManifest(reordered);
  assert.ok(
    problems.some((problem) => problem.includes("which no earlier producer writes")),
    `expected an ordering problem, got: ${JSON.stringify(problems)}`
  );
});

test("a producer must not run a guard for output written later in the plan", () => {
  // The regression this encodes: a corpus freshness test added to the same
  // cargo test module a snapshot producer regenerates. The producer's own run
  // tripped the downstream guard and aborted `make tool-goldens` before the
  // step that would have satisfied it.
  assert.deepEqual(producersRunningDownstreamGuards(manifest), []);

  const laterProducer = manifest.producers.at(-1);
  const earlierShell = manifest.producers.find(
    (producer) => producer.kind === "shell" && producer.id !== laterProducer.id
  );
  const trap = {
    ...manifest,
    artifacts: [
      ...manifest.artifacts,
      {
        path: "server/late.json",
        producer: laterProducer.id,
        guard: {
          file: "Makefile",
          runs: `${earlierShell.command.replace(" --locked", "")}::some_downstream_check`,
        },
      },
    ],
  };
  assert.deepEqual(producersRunningDownstreamGuards(trap), [
    `producer ${earlierShell.id} runs '${trap.artifacts.at(-1).guard.runs}', which guards output of the later producer ${laterProducer.id}`,
  ]);
});

test("validation rejects an artifact whose producer does not exist", () => {
  const broken = {
    ...manifest,
    artifacts: [...manifest.artifacts, { path: "x.json", producer: "nope", guard: { file: "Makefile", runs: "x" } }],
  };
  assert.ok(validateManifest(broken).some((problem) => problem.includes("unknown producer: nope")));
});

test("validation rejects an artifact with no regeneration hint declared", () => {
  const broken = {
    ...manifest,
    artifacts: [...manifest.artifacts, { path: "x.json", producer: manifest.producers[0].id }],
  };
  const problems = validateManifest(broken);
  assert.ok(problems.some((problem) => problem.includes("needs a guard.file")));
  assert.ok(problems.some((problem) => problem.includes("needs a guard.runs")));
});

// ── glob matching ──────────────────────────────────────────────────────────

test("glob matching handles the forms the manifest uses", () => {
  assert.equal(matchesGlob("a/b/c_tool_schemas.snap", "a/b/*_tool_schemas.snap"), true);
  assert.equal(matchesGlob("a/b/c/d.snap", "a/b/*.snap"), false);
  assert.equal(matchesGlob("a/b/c/d.snap", "a/**/d.snap"), true);
  assert.equal(matchesGlob("a/worker.json", "a/{worker,planner}.json"), true);
  assert.equal(matchesGlob("a/reviewer.json", "a/{worker,planner}.json"), false);
  // A literal dot must not behave as the regex wildcard.
  assert.equal(matchesGlob("a/bxjson", "a/b.json"), false);
});

// ── discovery and classification against the real tree ─────────────────────

test("every discovered candidate is declared or explicitly not derived", () => {
  const result = evaluate(manifest);
  assert.equal(result.indeterminate, false);
  assert.deepEqual(
    result.unlisted.map((candidate) => candidate.path),
    []
  );
  assert.ok(result.counts.declared > 0);
});

test("discovery is inconclusive rather than failing when a root is missing", () => {
  // The fail-closed version of this guard would wedge the merge queue on any
  // checkout it cannot fully see. It must warn instead.
  const empty = mkdtempSync(path.join(os.tmpdir(), "tool-goldens-empty-"));
  try {
    const result = evaluate(manifest, empty);
    assert.equal(result.indeterminate, true);
    assert.deepEqual(result.missingRoots, manifest.discovery.roots);
    assert.deepEqual(result.unlisted, []);
  } finally {
    rmSync(empty, { recursive: true, force: true });
  }
});

test("an undeclared derived artifact is reported as unlisted", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "tool-goldens-tree-"));
  try {
    const dir = path.join(root, "server", "crates", "made-up", "tests", "snapshots");
    mkdirSync(dir, { recursive: true });
    writeFileSync(
      path.join(dir, "made_up__new_tool_schemas.snap"),
      '---\nsource: x\n---\n[{"name":"x","inputSchema":{}}]\n'
    );
    mkdirSync(path.join(root, "ui", "src"), { recursive: true });

    const { candidates, missingRoots } = discoverCandidates(manifest, root);
    assert.deepEqual(missingRoots, []);
    const { unlisted } = classifyCandidates(manifest, candidates);
    assert.deepEqual(
      unlisted.map((candidate) => candidate.path),
      ["server/crates/made-up/tests/snapshots/made_up__new_tool_schemas.snap"]
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("hand-authored corpus inputs stay classified as not derived", () => {
  const result = evaluate(manifest);
  const excluded = result.excluded.map((entry) => entry.path);
  assert.ok(
    excluded.some((rel) => rel.endsWith("tool_schema_projection/regression/01_empty_items_object.json")),
    `regression fixtures must be excluded, got: ${JSON.stringify(excluded)}`
  );
  for (const entry of result.excluded) {
    assert.ok(entry.reason.length > 0, `${entry.path} needs a reason`);
  }
});

test("a declared pattern that matches nothing warns instead of failing", () => {
  const withGhost = {
    ...manifest,
    artifacts: [
      ...manifest.artifacts,
      {
        path: "server/does/not/exist.json",
        producer: manifest.producers[0].id,
        guard: { file: "Makefile", runs: "make tool-goldens-check" },
      },
    ],
  };
  const { scanned } = discoverCandidates(withGhost);
  assert.deepEqual(staleArtifactPatterns(withGhost, scanned), ["server/does/not/exist.json"]);
  // ...and it does not turn into a failure.
  assert.equal(evaluate(withGhost).ok, true);
});

// ── every guard names the command ──────────────────────────────────────────

test("every guard file names the regeneration command", () => {
  assert.deepEqual(findSilentGuards(manifest), []);
});

test("the silent-guard check actually reads the guard file", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "tool-goldens-guard-"));
  try {
    const single = { ...manifest, artifacts: [manifest.artifacts[0]] };
    const guardFile = path.join(root, manifest.artifacts[0].guard.file);
    mkdirSync(path.dirname(guardFile), { recursive: true });
    writeFileSync(guardFile, "// a guard that tells the author nothing\n");
    assert.deepEqual(findSilentGuards(single, root), [
      { file: manifest.artifacts[0].guard.file, missing: false },
    ]);

    writeFileSync(guardFile, `// run ${manifest.regenerateCommand}\n`);
    assert.deepEqual(findSilentGuards(single, root), []);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// ── idempotence of the pure derivation step ────────────────────────────────

test("insta frontmatter stripping is idempotent and newline-canonical", () => {
  const raw = '---\nsource: a.rs\nexpression: b()\n---\n[\n  1\n]\n';
  const once = instaSnapshotBody(raw);
  assert.equal(once, "[\n  1\n]\n");
  assert.equal(instaSnapshotBody(once), once);
  // Trailing-whitespace variants converge on the same bytes, so a second
  // `make tool-goldens` cannot produce a diff.
  assert.equal(instaSnapshotBody(`${raw}\n\n`), once);
});

test("the derive step reproduces the committed corpus fixtures byte for byte", () => {
  // This is the cross-crate copy that `make tool-goldens` performs. Running it
  // against the committed inputs must reproduce the committed outputs exactly;
  // if it does not, the target is not idempotent on a clean tree.
  const derive = manifest.producers.find((producer) => producer.kind === "derive-insta-body");
  assert.ok(derive, "the manifest must declare a derive-insta-body producer");
  for (const file of derive.files) {
    const generated = instaSnapshotBody(readFileSync(path.join(repoRoot, file.from), "utf8"));
    const committed = readFileSync(path.join(repoRoot, file.to), "utf8");
    assert.equal(generated, committed, `${file.to} is not what ${file.from} derives to`);
  }
});

// ── the plan the Makefile drives ───────────────────────────────────────────

test("the full plan runs every producer in manifest order", () => {
  const plan = buildPlan(manifest);
  assert.deepEqual(
    plan.map((step) => step.id),
    manifest.producers.map((producer) => producer.id)
  );
  for (const step of plan) {
    assert.ok(step.writes.length > 0, `${step.id} writes nothing`);
  }
});

test("selecting an unknown producer is an error, not a silent no-op", () => {
  assert.throws(() => buildPlan(manifest, ["not-a-producer"]), /unknown producer id/);
});

test("the golden pipeline enables the provisioned pnpm toolchain", () => {
  const pnpm = resolvePnpmCommand({ PNPM_HOME: "/cache/pnpm", PATH: "" });
  assert.equal(pnpm, "/cache/pnpm/bin/pnpm");
  assert.equal(withPnpmOnPath({ PNPM_HOME: "/cache/pnpm", PATH: "" }).PATH, "/cache/pnpm/bin");
});

test("the git pathspec covers every declared artifact pattern", () => {
  const paths = goldenPaths(manifest);
  for (const artifact of manifest.artifacts) {
    for (const pattern of entryPatterns(artifact)) {
      assert.ok(
        paths.some((rel) => matchesGlob(rel, pattern)),
        `${pattern} contributes nothing to the tool-goldens-check pathspec`
      );
    }
  }
});
