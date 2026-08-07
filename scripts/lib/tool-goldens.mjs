// Shared loader / matcher / discovery for the MCP tool-schema golden manifest.
//
// Consumers:
//   scripts/regenerate-tool-goldens.mjs  — runs every producer (`make tool-goldens`)
//   scripts/check-tool-goldens.mjs       — completeness guard (`make tool-goldens-check`)
//   scripts/tool-goldens.test.mjs        — contract tests for both of the above
//
// Everything here is pure except `discoverCandidates`, which only reads the
// working tree. Nothing in this module executes a producer.

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** Repository root, derived from this file's own location. */
export const repoRoot = path.resolve(__dirname, "..", "..");

export const MANIFEST_PATH = path.join(repoRoot, "scripts", "tool-goldens.manifest.json");

/**
 * Expand `{a,b}` alternations, then translate the remaining glob syntax to a
 * RegExp. `**` crosses directory separators; `*` does not.
 */
function expandBraces(pattern) {
  const open = pattern.indexOf("{");
  if (open === -1) return [pattern];
  const close = pattern.indexOf("}", open);
  if (close === -1) return [pattern];
  const head = pattern.slice(0, open);
  const tail = pattern.slice(close + 1);
  return pattern
    .slice(open + 1, close)
    .split(",")
    .flatMap((alt) => expandBraces(`${head}${alt}${tail}`));
}

function globToRegExp(pattern) {
  // Hand-rolled rather than sentinel-substituted: a sentinel that survives into
  // the output would silently change what the guard matches.
  let source = "";
  let index = 0;
  while (index < pattern.length) {
    const char = pattern[index];
    if (char !== "*") {
      source += char.replace(/[.+^${}()|[\]\\?]/, "\\$&");
      index += 1;
      continue;
    }
    let stars = 0;
    while (pattern[index] === "*") {
      stars += 1;
      index += 1;
    }
    if (stars === 1) {
      source += "[^/]*";
    } else if (pattern[index] === "/") {
      // `**/` spans zero or more whole path segments.
      source += "(?:.*/)?";
      index += 1;
    } else {
      source += ".*";
    }
  }
  return new RegExp(`^${source}$`);
}

/** True when `relPath` (repo-relative, `/`-separated) matches `pattern`. */
export function matchesGlob(relPath, pattern) {
  return expandBraces(pattern).some((alt) => globToRegExp(alt).test(relPath));
}

/** Every repo-relative pattern an entry claims (`path` and/or `glob`). */
export function entryPatterns(entry) {
  const patterns = [];
  if (typeof entry.path === "string") patterns.push(entry.path);
  if (typeof entry.glob === "string") patterns.push(entry.glob);
  return patterns;
}

export function entryMatches(entry, relPath) {
  return entryPatterns(entry).some((pattern) => matchesGlob(relPath, pattern));
}

export function loadManifest(manifestPath = MANIFEST_PATH) {
  const raw = readFileSync(manifestPath, "utf8");
  const manifest = JSON.parse(raw);
  const problems = validateManifest(manifest);
  if (problems.length > 0) {
    throw new Error(
      `${path.relative(repoRoot, manifestPath)} is invalid:\n  - ${problems.join("\n  - ")}`
    );
  }
  return manifest;
}

/**
 * Structural validation. Returns a list of human-readable problems; empty means
 * valid. Kept separate from `loadManifest` so the tests can assert on rejected
 * shapes without writing files.
 */
export function validateManifest(manifest) {
  const problems = [];
  const require = (cond, message) => {
    if (!cond) problems.push(message);
  };

  require(manifest && typeof manifest === "object", "manifest must be an object");
  if (problems.length > 0) return problems;

  require(manifest.version === 1, "version must be 1");
  require(typeof manifest.regenerateCommand === "string", "regenerateCommand must be a string");
  require(typeof manifest.verifyCommand === "string", "verifyCommand must be a string");
  require(Array.isArray(manifest.producers), "producers must be an array");
  require(Array.isArray(manifest.artifacts), "artifacts must be an array");
  require(Array.isArray(manifest.notDerived), "notDerived must be an array");
  require(manifest.discovery && typeof manifest.discovery === "object", "discovery must be an object");
  if (problems.length > 0) return problems;

  const producerIds = new Set();
  const produced = [];
  for (const producer of manifest.producers) {
    if (typeof producer.id !== "string" || producer.id.length === 0) {
      problems.push("every producer needs a non-empty id");
      continue;
    }
    if (producerIds.has(producer.id)) problems.push(`duplicate producer id: ${producer.id}`);
    producerIds.add(producer.id);
    if (typeof producer.description !== "string" || producer.description.length === 0) {
      problems.push(`producer ${producer.id} needs a description`);
    }
    if (producer.kind === "shell") {
      if (typeof producer.command !== "string" || producer.command.length === 0) {
        problems.push(`producer ${producer.id} (shell) needs a command`);
      }
      if (typeof producer.cwd !== "string") {
        problems.push(`producer ${producer.id} (shell) needs a cwd`);
      }
    } else if (producer.kind === "derive-insta-body") {
      if (!Array.isArray(producer.files) || producer.files.length === 0) {
        problems.push(`producer ${producer.id} (derive-insta-body) needs a non-empty files list`);
      } else {
        for (const file of producer.files) {
          if (typeof file.from !== "string" || typeof file.to !== "string") {
            problems.push(`producer ${producer.id} has a files entry missing from/to`);
          }
        }
      }
    } else {
      problems.push(`producer ${producer.id} has unknown kind: ${String(producer.kind)}`);
    }

    // Dependency order is the array order. A producer may only consume output
    // that an EARLIER producer already wrote, so one pass regenerates
    // everything and a second pass changes nothing.
    for (const consumed of producer.consumes ?? []) {
      if (!produced.some((pattern) => patternsOverlap(pattern, consumed))) {
        problems.push(
          `producer ${producer.id} consumes ${consumed}, which no earlier producer writes`
        );
      }
    }
    produced.push(...producerOutputs(manifest, producer.id));
  }

  const seen = new Map();
  for (const artifact of manifest.artifacts) {
    const patterns = entryPatterns(artifact);
    if (patterns.length === 0) {
      problems.push("every artifact needs a path or a glob");
      continue;
    }
    for (const pattern of patterns) {
      if (seen.has(pattern)) problems.push(`duplicate artifact pattern: ${pattern}`);
      seen.set(pattern, artifact);
    }
    if (!producerIds.has(artifact.producer)) {
      problems.push(`artifact ${patterns[0]} names unknown producer: ${String(artifact.producer)}`);
    }
    if (!artifact.guard || typeof artifact.guard.file !== "string") {
      problems.push(`artifact ${patterns[0]} needs a guard.file`);
    }
    if (!artifact.guard || typeof artifact.guard.runs !== "string") {
      problems.push(`artifact ${patterns[0]} needs a guard.runs`);
    }
  }

  const referenced = new Set(manifest.artifacts.map((artifact) => artifact.producer));
  for (const id of producerIds) {
    if (!referenced.has(id)) problems.push(`producer ${id} produces no declared artifact`);
  }

  problems.push(...producersRunningDownstreamGuards(manifest));

  for (const excluded of manifest.notDerived) {
    if (entryPatterns(excluded).length === 0) {
      problems.push("every notDerived entry needs a path or a glob");
    }
    if (typeof excluded.reason !== "string" || excluded.reason.length === 0) {
      problems.push(`notDerived entry ${entryPatterns(excluded)[0] ?? "?"} needs a reason`);
    }
  }

  const discovery = manifest.discovery;
  for (const field of ["roots", "ignoreDirs", "extensions", "contentMarkers", "pathMarkers"]) {
    if (!Array.isArray(discovery[field]) || discovery[field].length === 0) {
      problems.push(`discovery.${field} must be a non-empty array`);
    }
  }

  return problems;
}

function patternsOverlap(a, b) {
  return a === b || matchesGlob(b, a) || matchesGlob(a, b);
}

/** Cargo flags that do not change which tests a command selects. */
function normalizeTestCommand(command) {
  return command.replace(/\s--locked\b/g, "").replace(/\s+/g, " ").trim();
}

/**
 * A producer must not run a guard that checks an artifact written LATER in the
 * plan: the guard would fail on the very staleness the plan is about to fix,
 * and abort the run. This caught exactly that — a corpus freshness test added
 * to the same cargo test module a snapshot producer regenerates, which wedged
 * `make tool-goldens` until the test moved to its own target.
 */
export function producersRunningDownstreamGuards(manifest) {
  const problems = [];
  manifest.producers.forEach((producer, index) => {
    if (producer.kind !== "shell") return;
    const selector = normalizeTestCommand(producer.command);
    for (const artifact of manifest.artifacts) {
      const guardIndex = manifest.producers.findIndex((other) => other.id === artifact.producer);
      if (guardIndex <= index) continue;
      const guarded = normalizeTestCommand(artifact.guard?.runs ?? "");
      if (guarded.startsWith(`${selector}::`) || guarded === selector) {
        problems.push(
          `producer ${producer.id} runs '${artifact.guard.runs}', which guards output of the later producer ${artifact.producer}`
        );
      }
    }
  });
  return problems;
}

/** Every declared output pattern of one producer. */
export function producerOutputs(manifest, producerId) {
  return manifest.artifacts
    .filter((artifact) => artifact.producer === producerId)
    .flatMap((artifact) => entryPatterns(artifact));
}

/**
 * Strip insta's YAML frontmatter and return the snapshot body with exactly one
 * trailing newline. Pure, and idempotent on its own output: a body that has no
 * frontmatter is returned unchanged apart from newline normalization.
 */
export function instaSnapshotBody(raw) {
  let body = raw;
  if (raw.startsWith("---")) {
    const parts = raw.split(/^---\s*$/m);
    if (parts.length >= 3) body = parts.slice(2).join("---").replace(/^\n+/, "");
  }
  return `${body.replace(/\s+$/, "")}\n`;
}

function walk(dir, ignoreDirs, out) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (ignoreDirs.includes(entry.name)) continue;
      walk(full, ignoreDirs, out);
    } else if (entry.isFile()) {
      out.push(full);
    }
  }
  return out;
}

/**
 * Walk `discovery.roots` and return every repo-relative path that looks like a
 * derived tool-schema artifact.
 *
 * Results are retained PER ROOT (`perRoot`) and only then aggregated. That
 * separation is the whole point: an absent root makes the artifact set for
 * THAT root unknown, and nothing more. Observations already made under the
 * roots that are present stay on the record.
 *
 * `missingRoots` is therefore an incomplete-coverage signal, not a licence to
 * discard evidence. A caller that finds it non-empty has not seen the whole
 * tree and must say so — but it must still act on what it did see.
 */
export function discoverCandidates(manifest, root = repoRoot) {
  const { roots, ignoreDirs, extensions, contentMarkers, pathMarkers } = manifest.discovery;
  const perRoot = [];

  const relativize = (abs) => path.relative(root, abs).split(path.sep).join("/");

  for (const declaredRoot of roots) {
    const abs = path.join(root, declaredRoot);
    if (!existsSync(abs) || !statSync(abs).isDirectory()) {
      perRoot.push({ root: declaredRoot, present: false, candidates: [], scanned: [] });
      continue;
    }

    const files = walk(abs, ignoreDirs, []);
    const candidates = [];
    for (const file of files) {
      const rel = relativize(file);
      if (!extensions.some((ext) => rel.endsWith(ext))) continue;

      let reason = null;
      if (pathMarkers.some((marker) => rel.includes(marker))) {
        reason = "path";
      } else {
        let content;
        try {
          content = readFileSync(file, "utf8");
        } catch {
          continue;
        }
        if (contentMarkers.some((marker) => content.includes(marker))) reason = "content";
      }
      if (reason) candidates.push({ path: rel, root: declaredRoot, reason });
    }

    candidates.sort((a, b) => a.path.localeCompare(b.path));
    perRoot.push({
      root: declaredRoot,
      present: true,
      candidates,
      scanned: files.map(relativize).sort(),
    });
  }

  const missingRoots = perRoot.filter((entry) => !entry.present).map((entry) => entry.root);
  const scanned = perRoot.flatMap((entry) => entry.scanned).sort();
  const candidates = perRoot
    .flatMap((entry) => entry.candidates)
    .sort((a, b) => a.path.localeCompare(b.path));

  return { candidates, missingRoots, scanned, perRoot };
}

/** True when `pattern` can only ever match inside `root`. */
export function patternUnderRoot(pattern, root) {
  return pattern === root || pattern.startsWith(`${root}/`);
}

/**
 * Classify discovered candidates against the manifest.
 *
 * Returns structured values only — the callers decide what to print and what
 * exit code to use.
 */
export function classifyCandidates(manifest, candidates) {
  const declared = [];
  const excluded = [];
  const unlisted = [];
  for (const candidate of candidates) {
    const artifact = manifest.artifacts.find((entry) => entryMatches(entry, candidate.path));
    if (artifact) {
      declared.push({ ...candidate, producer: artifact.producer });
      continue;
    }
    const notDerived = manifest.notDerived.find((entry) => entryMatches(entry, candidate.path));
    if (notDerived) {
      excluded.push({ ...candidate, reason: notDerived.reason });
      continue;
    }
    unlisted.push(candidate);
  }
  return { declared, excluded, unlisted };
}

/**
 * Artifact patterns that matched nothing on disk. Reported as a warning, never
 * a failure: a pattern can legitimately go empty between a deletion and the
 * manifest edit that follows it, and wedging the queue over that helps nobody.
 */
export function staleArtifactPatterns(manifest, scanned) {
  const stale = [];
  for (const artifact of manifest.artifacts) {
    for (const pattern of entryPatterns(artifact)) {
      if (!scanned.some((relPath) => matchesGlob(relPath, pattern))) {
        stale.push(pattern);
      }
    }
  }
  return stale;
}
