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
// Missing discovery roots are an INCOMPLETE-COVERAGE warning, not a verdict.
// A root this cannot scan makes that root's artifact set unknown and nothing
// else: unknown evidence is never converted into success, and known-bad
// evidence from a root that IS present is never discarded because some other
// root is unreadable. So `indeterminate` and `ok` are computed independently —
// a partial checkout still fails on a candidate it can prove is unregistered,
// and a checkout with no roots at all warns without inventing violations.
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
  patternUnderRoot,
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
  const { candidates, missingRoots, scanned, perRoot } = discoverCandidates(manifest, root);
  const { declared, excluded, unlisted } = classifyCandidates(manifest, candidates);
  const silentGuards = findSilentGuards(manifest, root);

  // A pattern that lives under a root nobody could scan did not "match
  // nothing" — it was never looked for. Reporting it as stale would tell an
  // author to delete a perfectly valid manifest entry.
  const stalePatterns = staleArtifactPatterns(manifest, scanned).filter(
    (pattern) => !missingRoots.some((missing) => patternUnderRoot(pattern, missing))
  );

  // `indeterminate` describes COVERAGE. `ok` describes what was OBSERVED.
  // They are deliberately independent: incomplete coverage never erases an
  // unregistered artifact found under a root that was present.
  const indeterminate = missingRoots.length > 0;

  return {
    indeterminate,
    missingRoots,
    scannedRoots: perRoot.filter((entry) => entry.present).map((entry) => entry.root),
    roots: perRoot.map((entry) => {
      const perRootClassification = entry.present
        ? classifyCandidates(manifest, entry.candidates)
        : { declared: [], excluded: [], unlisted: [] };
      return {
        root: entry.root,
        present: entry.present,
        counts: {
          candidates: entry.candidates.length,
          declared: perRootClassification.declared.length,
          excluded: perRootClassification.excluded.length,
          unlisted: perRootClassification.unlisted.length,
        },
        unlisted: perRootClassification.unlisted,
      };
    }),
    counts: {
      candidates: candidates.length,
      declared: declared.length,
      excluded: excluded.length,
      unlisted: unlisted.length,
    },
    declared,
    excluded,
    unlisted,
    silentGuards,
    stalePatterns,
    ok: unlisted.length === 0 && silentGuards.length === 0,
  };
}

function report(manifest, result) {
  for (const root of result.missingRoots) {
    console.warn(
      `::warning title=Tool-golden coverage incomplete::discovery root '${root}' is missing, ` +
        `so no artifact under it was inspected. Findings below cover only ` +
        `${result.scannedRoots.join(", ") || "no root"}.`
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
    const counts =
      `${result.counts.declared} declared artifact file(s), ` +
      `${result.counts.excluded} hand-authored file(s), 0 unregistered`;
    // Never claim full completeness off a partial scan.
    console.log(
      result.indeterminate
        ? `tool-schema goldens: ${counts} under ${result.scannedRoots.join(", ") || "no root"}. ` +
            `Coverage is INCOMPLETE — ${result.missingRoots.join(", ")} could not be scanned, ` +
            `so this is not a full inventory proof.`
        : `tool-schema goldens: ${counts}.`
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
