#!/usr/bin/env node
// Completeness guard for the MCP tool-schema golden manifest.
//
// The regeneration target is only as good as its artifact list. This walks the
// working tree for anything that LOOKS like a derived tool-schema artifact and
// fails if it appears in neither `artifacts` nor `notDerived` in
// scripts/tool-goldens.manifest.json — so the list cannot silently grow a
// seventh member the way it grew to six.
//
// It also asserts that every guard named in the manifest tells its author what
// to run. A check that only says "snapshot mismatched" costs an agent session
// and a CI cycle; that is the loop this whole mechanism exists to close.
//
// Deliberately NOT fail-closed: if the tree it was pointed at is incomplete
// (a discovery root is missing), it warns and exits 0. A guard that cannot see
// the tree must not wedge the merge queue.
//
// Flags:
//   --json     emit the structured result instead of human-readable output
//   --paths    print one repo-relative golden path per line and exit 0; this is
//              the pathspec `make tool-goldens-check` hands to `git diff`

import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

import {
  classifyCandidates,
  discoverCandidates,
  entryPatterns,
  loadManifest,
  matchesGlob,
  repoRoot,
  staleArtifactPatterns,
} from "./lib/tool-goldens.mjs";

/**
 * Every guard file must name the command that regenerates what it guards.
 * Returns the guards that do not.
 */
export function findSilentGuards(manifest, root = repoRoot) {
  const silent = [];
  const checked = new Set();
  for (const artifact of manifest.artifacts) {
    const guard = artifact.guard;
    const key = `${guard.file}::${artifact.producer}`;
    if (checked.has(key)) continue;
    checked.add(key);
    const abs = path.join(root, guard.file);
    if (!existsSync(abs)) {
      silent.push({ file: guard.file, missing: true });
      continue;
    }
    const source = readFileSync(abs, "utf8");
    if (!source.includes(manifest.regenerateCommand)) {
      silent.push({ file: guard.file, missing: false });
    }
  }
  return silent;
}

/** Pure classification of a tree. Callers own the printing and the exit code. */
export function evaluate(manifest, root = repoRoot) {
  const { candidates, missingRoots, scanned } = discoverCandidates(manifest, root);
  const { declared, excluded, unlisted } = classifyCandidates(manifest, candidates);
  const silentGuards = findSilentGuards(manifest, root);
  const stalePatterns = staleArtifactPatterns(manifest, scanned);

  // Indeterminate input is a warning, never a failure.
  const indeterminate = missingRoots.length > 0;
  const failures = indeterminate ? [] : [...unlisted];

  return {
    indeterminate,
    missingRoots,
    counts: {
      candidates: candidates.length,
      declared: declared.length,
      excluded: excluded.length,
      unlisted: unlisted.length,
    },
    declared,
    excluded,
    unlisted: failures,
    silentGuards,
    stalePatterns,
    ok: failures.length === 0 && silentGuards.length === 0,
  };
}

function report(manifest, result) {
  for (const root of result.missingRoots) {
    console.warn(
      `::warning title=Tool-golden guard inconclusive::discovery root '${root}' is missing, ` +
        `so the artifact set could not be determined. Not failing.`
    );
  }
  for (const pattern of result.stalePatterns) {
    console.warn(
      `::warning title=Tool-golden pattern matched nothing::'${pattern}' is declared in ` +
        `scripts/tool-goldens.manifest.json but matches no file. Remove it if the artifact is gone.`
    );
  }

  if (result.silentGuards.length > 0) {
    console.error(
      "::error title=Tool-golden guard gives no regeneration hint::" +
        `every guard listed in scripts/tool-goldens.manifest.json must name '${manifest.regenerateCommand}' ` +
        "in its failure path, so an author who trips it learns what to run:"
    );
    for (const guard of result.silentGuards) {
      console.error(
        guard.missing
          ? `  ${guard.file} (declared as a guard but not present)`
          : `  ${guard.file} (does not mention '${manifest.regenerateCommand}')`
      );
    }
  }

  if (result.unlisted.length > 0) {
    console.error(
      "::error title=Unregistered tool-schema artifact::" +
        "these committed files look like derived MCP tool-schema artifacts but are wired into " +
        "neither the regeneration target nor the not-derived list:"
    );
    for (const candidate of result.unlisted) {
      console.error(`  ${candidate.path}  (matched by ${candidate.reason})`);
    }
    console.error(
      "\nAdd each one to scripts/tool-goldens.manifest.json:\n" +
        `  - under 'artifacts', with the producer that writes it, so '${manifest.regenerateCommand}' refreshes it; or\n` +
        "  - under 'notDerived', with a reason, if it is hand-authored input rather than a projection\n" +
        "    of the live tool surface."
    );
  }

  if (result.ok) {
    console.log(
      `tool-schema goldens: ${result.counts.declared} declared artifact file(s), ` +
        `${result.counts.excluded} hand-authored file(s), 0 unregistered.`
    );
  }
}

/**
 * Every golden file the manifest claims, resolved against the tree. Literal
 * `path` entries are always included (even if absent, so `git diff` reports a
 * deletion); globs contribute whatever they currently match.
 */
export function goldenPaths(manifest, root = repoRoot) {
  const { scanned } = discoverCandidates(manifest, root);
  const paths = new Set();
  for (const artifact of manifest.artifacts) {
    if (typeof artifact.path === "string") paths.add(artifact.path);
    if (typeof artifact.glob === "string") {
      for (const rel of scanned) {
        if (matchesGlob(rel, artifact.glob)) paths.add(rel);
      }
    }
  }
  return [...paths].sort();
}

function main() {
  const args = process.argv.slice(2);
  const manifest = loadManifest();

  if (args.includes("--paths")) {
    for (const rel of goldenPaths(manifest)) console.log(rel);
    return;
  }

  const json = args.includes("--json");
  const result = evaluate(manifest);

  if (json) {
    console.log(JSON.stringify(result, null, 2));
  } else {
    report(manifest, result);
  }

  process.exit(result.ok ? 0 : 1);
}

if (process.argv[1] && import.meta.url === `file://${process.argv[1]}`) {
  main();
}

export { entryPatterns };
