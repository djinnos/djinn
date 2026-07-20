#!/usr/bin/env node
/**
 * Hermetic retirement manifest generator for Phase 1 knowledge retirement
 * (epic h1w2 / proposal qiy6).
 *
 * Consumes NUL-delimited `git ls-files -z` output plus an explicit hermetic
 * DB-selection/guidance fixture input and writes two deterministic JSON
 * manifests under `target/djinn-retirement/`:
 *
 *   - knowledge-manifest.json     — one entry per tracked `.djinn` knowledge
 *                                    file, with blob SHA-256, normalized-content
 *                                    SHA-256, detected permalink, selected DB
 *                                    identity, and exactly one disposition.
 *   - db-guidance-manifest.json   — one entry per affected DB guidance record,
 *                                    carrying selected identity, classification,
 *                                    rationale, status, hashes, and supersession
 *                                    linkage fields for the follow-up DB
 *                                    reconciliation task.
 *
 * Design constraints (see task mbfw):
 *   - Repository paths are fed as NUL-delimited bytes so spaces/newlines in
 *     filenames are handled without line splitting.
 *   - Committed blob SHA-256 is computed over the exact bytes `git show
 *     HEAD:<path>` returns (byte-exact), separately from normalized content.
 *   - Normalization may ONLY remove YAML front matter and canonicalize CRLF/CR
 *     to LF. No other content transformation is permitted.
 *   - DB access is behind an explicit JSON/fixture input contract so tests
 *     require no production credentials.
 *   - The tracked knowledge set is derived at runtime from the NUL-delimited
 *     input; the generator never hard-codes a baseline count.
 *
 * The generator is hermetic: it shells out to `git` only to read committed blob
 * bytes at an explicit revision (default HEAD). It never mutates the DB, never
 * deletes tracked files, and never performs Phase 2 live-state migration.
 */
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readSync,
  writeFileSync,
} from 'node:fs';
import { resolve } from 'node:path';
import { parseArgs } from 'node:util';

// ── Public data-contract types (documented for the follow-up DB task) ────────

/**
 * Allowed dispositions for a tracked knowledge file.
 *
 * - `equivalent`        — the file's normalized content matches a DB record's
 *                         normalized hash; the DB preserves the knowledge.
 * - `db_supersedes_file`— a DB record preserves the knowledge but the file's
 *                         normalized content differs (DB is authoritative).
 * - `approved_discard`  — the file's knowledge is explicitly approved for
 *                         discard; requires a non-empty reason and approving
 *                         task id.
 */
export const KNOWLEDGE_DISPOSITIONS = new Set([
  'equivalent',
  'db_supersedes_file',
  'approved_discard',
]);

/**
 * Knowledge families (folders / singletons) that constitute the project-local
 * tracked knowledge set. The sole non-knowledge tracked file under `.djinn/`
 * is `.gitignore`; retired operational paths remain knowledge-classified so
 * the post-cutover guard rejects them.
 */
export const KNOWLEDGE_FAMILIES = [
  'decisions',
  'reference',
  'research',
  'design',
  'requirements',
  'planning',
  'cases',
  'patterns',
  'pitfalls',
];

/** Singleton knowledge files rooted directly under `.djinn/`. */
export const KNOWLEDGE_SINGLETONS = new Set([
  '.djinn/brief.md',
  '.djinn/roadmap.md',
  '.djinn/catalog.md',
]);

/**
 * Non-knowledge tracked files under `.djinn/` that the generator deliberately
 * excludes from the knowledge set. They are operational/generated artifacts,
 * not knowledge, and are owned by other retirement tasks.
 */
export const NON_KNOWLEDGE_TRACKED = new Set();

/**
 * Operational paths that were intentionally removed during Phase 2 retirement
 * and must never be reintroduced. The post-cutover guard rejects any current
 * tracked file matching one of these paths.
 */
export const RETIRED_OPERATIONAL_PATHS = new Set([
  '.djinn/.gitignore',
  '.djinn/settings.json',
  '.djinn/skills.json',
]);

/** Default output directory (transient; must be gitignored). */
export const DEFAULT_OUTPUT_DIR = 'target/djinn-retirement';

const NON_KNOWLEDGE_REASON = 'file is operational/generated, not a knowledge artifact';

// ── Errors ───────────────────────────────────────────────────────────────────

export class ManifestError extends Error {
  constructor(message, { code, entry } = {}) {
    super(message);
    this.name = 'ManifestError';
    this.code = code || 'manifest_error';
    if (entry) this.entry = entry;
  }
}

export function assertNoProjectLocalDjinnSurface(candidates) {
  for (const candidate of candidates) {
    if (!candidate || typeof candidate.path !== 'string') continue;
    if (isStructurallyAllowedSurfaceCandidate(candidate)) continue;
    const path = candidate.path.split('\\').join('/');
    if (path === '~/.djinn' || path.startsWith('~/.djinn/') ||
        path === '$DJINN_HOME/.djinn' || path.startsWith('$DJINN_HOME/.djinn/')) continue;
    if (path === '.djinn' || path.startsWith('.djinn/') || path.includes('/.djinn/') ||
        path.endsWith('/.djinn') || path.includes('/~/.djinn') || path.includes('/$DJINN_HOME/.djinn')) {
      throw new ManifestError(`project-local .djinn surface was reintroduced: ${path}`, {
        code: 'project_local_surface_reintroduced', entry: { repository_path: path },
      });
    }
  }
}

export function isStructurallyAllowedSurfaceCandidate(candidate) {
  const origin = typeof candidate?.repository_path === 'string'
    ? candidate.repository_path.split('\\').join('/') : '';
  const surface = typeof candidate?.path === 'string'
    ? candidate.path.split('\\').join('/') : '';
  // The workspace handoff is the sole cleanup boundary permitted to inspect
  // the retired settings source. This exact repository location and retired
  // path shape are required; callers cannot bypass the policy with a `kind`.
  if (origin === 'server/crates/djinn-workspace/src/legacy_settings_import.rs' &&
      surface === '.djinn/settings.json') return true;
  // The residue gate is the matching no-follow cleanup boundary for the
  // retired directory itself; it inspects and removes only legacy residue.
  if (origin === 'server/crates/djinn-workspace/src/project_residue.rs' &&
      surface === '.djinn') return true;
  // The live-state migration record is immutable evidence of the retired
  // read-source location, not a runtime producer or consumer.
  if (origin === 'server/crates/djinn-db/src/repositories/project_live_state_migration.rs' &&
      surface === '.djinn/read-sources/target-a') return true;
  // The ledger and its generated fixtures are immutable deletion evidence.
  // Test sources are allowed only through the structural test-range rule below.
  return origin === 'scripts/djinn-retirement-manifest.mjs' ||
    origin === 'scripts/test-djinn-retirement-manifest.mjs' ||
    origin.startsWith('scripts/fixtures/djinn-retirement/');
}

// Whole-file negative-test exemptions are deliberately limited to the exact
// repository fixtures which exercise this retirement policy. Test-like names
// elsewhere are not policy: production code can legitimately live in files
// named `test-*` or `*.test.*`, so their contents must remain scanned.
const NEGATIVE_TEST_FIXTURE_PATHS = new Set([
  'scripts/ci-changed-scope.test.mjs',
  'scripts/test-djinn-retirement-manifest.mjs',
]);

function rustSyntaxMask(text) {
  // Keep a byte-level map of Rust syntax, excluding comments and literals.
  // This is intentionally a lexer rather than a Rust parser: the structural
  // allowance only needs trustworthy attribute and brace locations.
  const syntax = new Uint8Array(text.length).fill(1);
  const exclude = (start, end) => syntax.fill(0, start, Math.min(end, text.length));
  for (let index = 0; index < text.length; index += 1) {
    if (text.startsWith('//', index)) {
      const end = text.indexOf('\n', index + 2);
      exclude(index, end < 0 ? text.length : end);
      index = end < 0 ? text.length : end;
      continue;
    }
    if (text.startsWith('/*', index)) {
      const start = index;
      let depth = 1;
      index += 2;
      while (index < text.length && depth > 0) {
        if (text.startsWith('/*', index)) { depth += 1; index += 2; }
        else if (text.startsWith('*/', index)) { depth -= 1; index += 2; }
        else index += 1;
      }
      exclude(start, index);
      index -= 1;
      continue;
    }
    const rawStart = text[index] === 'r' ? index + 1 :
      (text[index] === 'b' && text[index + 1] === 'r' ? index + 2 : null);
    if (rawStart !== null) {
      let delimiter = rawStart;
      while (text[delimiter] === '#') delimiter += 1;
      if (text[delimiter] === '"') {
        const terminator = `"${'#'.repeat(delimiter - rawStart)}`;
        const end = text.indexOf(terminator, delimiter + 1);
        const after = end < 0 ? text.length : end + terminator.length;
        exclude(index, after);
        index = after - 1;
        continue;
      }
    }
    if (text[index] === '"' || (text[index] === 'b' && text[index + 1] === '"')) {
      const start = index;
      index += text[index] === 'b' ? 2 : 1;
      while (index < text.length) {
        if (text[index] === '\\') index += 2;
        else if (text[index++] === '"') break;
      }
      exclude(start, index);
      index -= 1;
      continue;
    }
    if (text[index] === "'" &&
        (text[index + 2] === "'" || (text[index + 1] === '\\' && text[index + 3] === "'"))) {
      const end = index + (text[index + 1] === '\\' ? 4 : 3);
      exclude(index, end);
      index = end - 1;
    }
  }
  return syntax;
}

function closingRustModuleBrace(text, syntax, openingBrace) {
  let depth = 1;
  for (let index = openingBrace + 1; index < text.length; index += 1) {
    if (!syntax[index]) continue;
    if (text[index] === '{') depth += 1;
    if (text[index] === '}') {
      depth -= 1;
      if (depth === 0) return index + 1;
    }
  }
  return null;
}
export function discoverProjectLocalDjinnSurfaces(trackedPaths, opts = {}) {
  const cwd = opts.cwd || process.cwd();
  const candidates = [];
  // Scan path literals only where their surrounding syntax makes them a
  // runtime/scaffolding operation, rather than treating historical prose or a
  // bare `.djinn` token as a policy violation. A join receiver is deliberately
  // not name-limited: `project_root.join`, `root.join`, and a path expression
  // passed to `create_dir_all` all construct the same retired project surface.
  // The server-home construction is normalized to its immediately rooted
  // namespace before the shared structural policy evaluates it.
  const joinedPath = /(?<receiver>\b(?:[A-Za-z_][A-Za-z0-9_]*|(?:(?:std\s*::\s*path\s*::\s*)?(?:PathBuf|Path))\s*::\s*from\s*\([^\r\n)]*\)))\s*\.\s*join\(\s*(?<quote>["'])(?<path>\.djinn(?:\/[^"'\r\n\s]*)?)\k<quote>/g;
  const pathConstructor = /\b(?:(?:std\s*::\s*path\s*::\s*)?(?:PathBuf|Path))\s*::\s*(?:from|new)\(\s*(["'])(\.djinn(?:\/[^"'\r\n\s]*)?)\1/g;
  const directFilesystemOperation = /\b(?:(?:std\s*::\s*fs|fs|node:fs|tokio\s*::\s*fs)\s*::\s*|(?:fs\s*\.\s*)?)(?:read(?:_to_string|_dir)?|write|create(?:_dir(?:_all)?|_new)?|open|metadata|remove_(?:file|dir|dir_all)|rename|copy)\s*\(\s*(["'])(\.djinn(?:\/[^"'\r\n\s]*)?)\1/g;
  // Constants are intentional path declarations; this covers the one-time
  // cleanup handoff without classifying prose or arbitrary string literals.
  const declaredPath = /\b(?:const|let|static)\s+[A-Za-z_][A-Za-z0-9_]*[^=\r\n]*=\s*(["'])(\.djinn(?:\/[^"'\r\n\s]*)?)\1/g;
  const configuredPath = /(?:[:=]\s*)(?:(["'])(\.djinn(?:\/[^\s,}\]]*)?)\1|(\.djinn(?:\/[^\s,}\]]*)?))/g;
  const indexBlobs = new Map();
  if (!opts.readIndexBlob) {
    const ordinaryPaths = trackedPaths.filter((path) => !path.includes('\n'));
    if (ordinaryPaths.length > 0) {
      try {
        const batch = execFileSync(opts.git || 'git', ['cat-file', '--batch'], {
          cwd,
          input: Buffer.from(ordinaryPaths.map((path) => `:${path}\n`).join('')),
          maxBuffer: 64 * 1024 * 1024,
        });
        let offset = 0;
        for (const path of ordinaryPaths) {
          const newline = batch.indexOf(10, offset);
          if (newline < 0) break;
          const header = batch.subarray(offset, newline).toString('utf8');
          offset = newline + 1;
          const match = /^[0-9a-f]+ blob (\d+)$/.exec(header);
          if (!match) continue;
          const size = Number(match[1]);
          indexBlobs.set(path, batch.subarray(offset, offset + size));
          offset += size + 1;
        }
      } catch {
        // Fall back to individual index reads below when batch lookup fails.
      }
    }
  }
  for (const repositoryPath of trackedPaths) {
    let bytes;
    try {
      // The policy input is the staged blob, not a potentially divergent
      // worktree file. Tests may inject an equivalent blob reader.
      bytes = opts.readIndexBlob
        ? opts.readIndexBlob(repositoryPath)
        : indexBlobs.get(repositoryPath) || execFileSync(opts.git || 'git', ['show', `:${repositoryPath}`], { cwd, maxBuffer: 64 * 1024 * 1024 });
    } catch {
      continue;
    }
    if (bytes.includes(0)) continue;
    const text = bytes.toString('utf8');
    // An exemption is bounded to a cfg(test) module's braces, so production
    // code after a test module cannot inherit the exemption.
    const testRanges = [];
    let syntax;
    for (const marker of text.matchAll(/#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{/g)) {
      syntax ||= rustSyntaxMask(text);
      // A lookalike in prose, a comment, or a literal is not an attribute.
      if (![...marker[0]].every((_, offset) => syntax[marker.index + offset])) continue;
      const end = closingRustModuleBrace(text, syntax, marker.index + marker[0].length - 1);
      if (end !== null) testRanges.push([marker.index, end]);
    }
    const isNegativeTestLocation = (offset) =>
      NEGATIVE_TEST_FIXTURE_PATHS.has(repositoryPath) ||
      testRanges.some(([start, end]) => offset >= start && offset < end);
    for (const match of text.matchAll(joinedPath)) {
      if (isNegativeTestLocation(match.index)) continue;
      const receiver = match.groups.receiver.replace(/\s/g, '');
      const surface = /(?:^|[(&])home\)?$/i.test(receiver)
        ? `$DJINN_HOME/${match.groups.path}` : match.groups.path;
      candidates.push({ path: surface, repository_path: repositoryPath });
    }
    for (const match of text.matchAll(pathConstructor)) {
      if (!isNegativeTestLocation(match.index)) candidates.push({ path: match[2], repository_path: repositoryPath });
    }
    for (const match of text.matchAll(directFilesystemOperation)) {
      if (!isNegativeTestLocation(match.index)) candidates.push({ path: match[2], repository_path: repositoryPath });
    }
    for (const match of text.matchAll(declaredPath)) {
      if (!isNegativeTestLocation(match.index)) candidates.push({ path: match[2], repository_path: repositoryPath });
    }
    for (const match of text.matchAll(configuredPath)) {
      if (!isNegativeTestLocation(match.index)) candidates.push({ path: match[2] || match[3], repository_path: repositoryPath });
    }
  }
  return candidates;
}

// ── Normalization ────────────────────────────────────────────────────────────

/**
 * Strip a leading YAML front-matter block delimited by `---` lines and
 * canonicalize line endings (CRLF and lone CR → LF).
 *
 * Returns the normalized content as a Buffer (byte-exact for hashing).
 *
 * Normalization is intentionally minimal: only front-matter removal and
 * line-ending canonicalization. No whitespace trimming, no Unicode folding,
 * no content mutation.
 */
export function normalizeContent(bytes) {
  const crlfToLf = bytes.toString('binary').replace(/\r\n/g, '\n').replace(/\r/g, '\n');
  // Strip a leading YAML front-matter block: `^---\n ... \n---\n`.
  const fm = /^---\r?\n[\s\S]*?\r?\n---\r?\n/;
  const stripped = crlfToLf.replace(fm, '');
  return Buffer.from(stripped, 'binary');
}

// ── Hashing ──────────────────────────────────────────────────────────────────

/** Compute the SHA-256 hex digest of a Buffer. */
export function sha256Hex(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

// ── Path filtering ───────────────────────────────────────────────────────────

/**
 * Decide whether a tracked repository path belongs to the project-local
 * `.djinn/` knowledge set.
 *
 * non-knowledge operational/generated file (`.gitignore`). Retired operational
 * paths remain classified here so a current tracked path is rejected by the
 * same guard; source-revision ledger validation excludes them explicitly.
 */
export function isKnowledgePath(repoPath) {
  if (typeof repoPath !== 'string' || repoPath.length === 0) return false;
  // Normalize separators so a Windows-style path still classifies correctly.
  const norm = repoPath.split('\\').join('/');
  if (!norm.startsWith('.djinn/')) return false;
  // Reject bare directory paths (must be a file, not a folder).
  if (norm.endsWith('/')) return false;
  if (norm === '.djinn/.gitignore' || norm === '.djinn/skills.json' ||
      NON_KNOWLEDGE_TRACKED.has(norm)) return false;
  return true;
}

/**
 * Split NUL-delimited path bytes into an array of non-empty path strings.
 *
 * NUL-delimited input is required so that paths containing spaces or newlines
 * (the `git ls-files -z` contract) survive without line splitting.
 */
export function splitNulPaths(bytes) {
  const out = [];
  let start = 0;
  for (let i = 0; i < bytes.length; i += 1) {
    if (bytes[i] === 0) {
      if (i > start) out.push(bytes.subarray(start, i).toString('utf8'));
      start = i + 1;
    }
  }
  if (start < bytes.length) {
    out.push(bytes.subarray(start).toString('utf8'));
  }
  return out;
}

// ── Permalink detection ──────────────────────────────────────────────────────

/**
 * Detect the canonical DB permalink for a tracked knowledge path.
 *
 * Knowledge files historically lived under `.djinn/<folder>/<slug>.md` where
 * `<folder>` is the note family (`decisions`, `reference`, ...) and `<slug>`
 * is the note slug. The DB permalink is `<folder>/<slug>` (or `<slug>` for
 * root singletons `brief`, `roadmap`, `catalog`).
 *
 * Special handling: `research/technical/*` maps to the `tech_spike` type whose
 * DB folder is also `research/technical`.
 */
export function detectPermalink(repoPath) {
  const norm = repoPath.split('\\').join('/');
  // Legacy settings are a retired source artifact recorded in the durable ledger.
  if (norm === '.djinn/settings.json') return 'settings';
  if (!norm.startsWith('.djinn/') || !norm.endsWith('.md')) {
    return null;
  }
  const rel = norm.slice('.djinn/'.length);
  // Drop the `.md` suffix.
  const withoutExt = rel.slice(0, -'.md'.length);
  // Root singletons: brief, roadmap, catalog → permalink is the bare name.
  const slash = withoutExt.indexOf('/');
  if (slash === -1) {
    return withoutExt;
  }
  // Nested: keep the full `<folder>/<...>/<slug>` as the permalink. The DB
  // permalink scheme preserves intermediate segments (e.g.
  // `research/technical/<slug>`).
  return withoutExt;
}

// ── Committed blob reading ───────────────────────────────────────────────────

/**
 * Read the committed blob bytes for `repoPath` at `revision` (byte-exact).
 *
 * Uses `git show <revision>:<path>` which returns the exact stored blob.
 * Throws if git is unavailable or the path is not tracked at `revision`.
 */
export function readCommittedBlob(repoPath, revision, { git = 'git', cwd } = {}) {
  const ref = `${revision}:${repoPath}`;
  try {
    return execFileSync(git, ['show', ref], {
      cwd,
      maxBuffer: 64 * 1024 * 1024,
    });
  } catch (err) {
    throw new ManifestError(
      `failed to read committed blob for ${repoPath} at ${revision}: ${err.message}`,
      { code: 'blob_read_error', entry: { repository_path: repoPath } },
    );
  }
}

// ── DB selection fixture contract ────────────────────────────────────────────

/**
 * Validate the hermetic DB-selection fixture shape.
 *
 * The fixture is an explicit JSON input contract so generator tests require no
 * production credentials. It maps detected permalinks to a DB record selection
 * carrying uuid, permalink, status, normalized-content SHA-256, and (for
 * disambiguation) a confidence value.
 *
 * Shape:
 *   {
 *     "schema": "djinn-retirement-db-selection/v1",
 *     "records": {
 *       "<permalink>": {
 *         "uuid": "...",
 *         "permalink": "...",
 *         "status": "active|archived|deprecated",
 *         "normalized_sha256": "<hex>",
 *         "confidence": 0.0..1.0
 *       }
 *     }
 *   }
 *
 * A permalink may map to exactly ONE record. The generator rejects ambiguous
 * matches: if a permalink maps to more than one candidate record the fixture
 * itself is malformed.
 */
export function loadDbSelectionFixture(fixturePath) {
  if (!fixturePath) {
    // No fixture => every knowledge file is unresolved by default.
    return { schema: null, records: new Map() };
  }
  const abs = resolve(fixturePath);
  if (!existsSync(abs)) {
    throw new ManifestError(`DB selection fixture not found: ${abs}`, {
      code: 'fixture_missing',
    });
  }
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(abs, 'utf8'));
  } catch (err) {
    throw new ManifestError(`DB selection fixture is not valid JSON: ${err.message}`, {
      code: 'fixture_invalid_json',
    });
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new ManifestError('DB selection fixture must be a JSON object', {
      code: 'fixture_shape',
    });
  }
  if (parsed.schema !== 'djinn-retirement-db-selection/v1') {
    throw new ManifestError(
      `DB selection fixture has unsupported schema: ${JSON.stringify(parsed.schema)}`,
      { code: 'fixture_schema' },
    );
  }
  if (!parsed.records || typeof parsed.records !== 'object' || Array.isArray(parsed.records)) {
    throw new ManifestError('DB selection fixture `records` must be an object', {
      code: 'fixture_shape',
    });
  }
  const records = new Map();
  for (const [permalink, rec] of Object.entries(parsed.records)) {
    if (!rec || typeof rec !== 'object' || Array.isArray(rec)) {
      throw new ManifestError(
        `DB selection record for ${permalink} must be an object`,
        { code: 'fixture_record_shape', entry: { permalink } },
      );
    }
    if (!rec.uuid || typeof rec.uuid !== 'string') {
      throw new ManifestError(
        `DB selection record for ${permalink} is missing a non-empty uuid`,
        { code: 'fixture_record_uuid', entry: { permalink } },
      );
    }
    if (typeof rec.permalink !== 'string' || rec.permalink.length === 0) {
      throw new ManifestError(
        `DB selection record for ${permalink} is missing a non-empty permalink`,
        { code: 'fixture_record_permalink', entry: { permalink } },
      );
    }
    const status = typeof rec.status === 'string' ? rec.status : 'active';
    if (!['active', 'archived', 'deprecated'].includes(status)) {
      throw new ManifestError(
        `DB selection record for ${permalink} has invalid status: ${status}`,
        { code: 'fixture_record_status', entry: { permalink } },
      );
    }
    if (typeof rec.normalized_sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(rec.normalized_sha256)) {
      throw new ManifestError(
        `DB selection record for ${permalink} is missing a valid normalized_sha256`,
        { code: 'fixture_record_hash', entry: { permalink } },
      );
    }
    const confidence = typeof rec.confidence === 'number' ? rec.confidence : 1.0;
    if (confidence < 0 || confidence > 1) {
      throw new ManifestError(
        `DB selection record for ${permalink} has out-of-range confidence: ${confidence}`,
        { code: 'fixture_record_confidence', entry: { permalink } },
      );
    }
    records.set(permalink, {
      uuid: rec.uuid,
      permalink: rec.permalink,
      status,
      normalized_sha256: rec.normalized_sha256,
      confidence,
    });
  }
  return { schema: parsed.schema, records };
}

// ── Knowledge entry assembly ─────────────────────────────────────────────────

/**
 * Build a single knowledge manifest entry for `repoPath`.
 *
 * Reads the committed blob, computes both hashes, detects the permalink, looks
 * up the DB selection, and assigns exactly one disposition.
 */
export function buildKnowledgeEntry(repoPath, revision, dbSelection, opts = {}) {
  const git = opts.git || 'git';
  const cwd = opts.cwd;
  const seenPermalinks = opts.seenPermalinks || new Map();

  const norm = repoPath.split('\\').join('/');

  // Detect permalink.
  const permalink = detectPermalink(norm);

  // Read committed blob (byte-exact) and compute blob SHA-256.
  const blob = readCommittedBlob(norm, revision, { git, cwd });
  const blobSha256 = sha256Hex(blob);

  // Normalize and compute normalized-content SHA-256.
  const normalized = normalizeContent(blob);
  const normalizedSha256 = sha256Hex(normalized);

  // Look up DB selection.
  let db;
  if (permalink) {
    const rec = dbSelection.records.get(permalink);
    if (rec) {
      db = {
        uuid: rec.uuid,
        permalink: rec.permalink,
        status: rec.status,
        normalized_sha256: rec.normalized_sha256,
      };
    }
  }

  // Determine disposition.
  let disposition;
  let rationale;
  let approvingTaskId;
  if (db) {
    if (db.normalized_sha256 === normalizedSha256) {
      disposition = 'equivalent';
      rationale = 'DB record normalized-content hash matches the committed file';
    } else {
      disposition = 'db_supersedes_file';
      rationale = 'DB record preserves the knowledge but normalized content differs';
    }
  } else {
    // No DB selection => unresolved unless explicitly approved for discard.
    disposition = 'approved_discard';
    rationale = NON_KNOWLEDGE_REASON;
    approvingTaskId = null;
  }

  // Validate discard fields.
  if (disposition === 'approved_discard') {
    if (!rationale || rationale.trim().length === 0) {
      throw new ManifestError(
        `approved_discard entry for ${norm} has an empty reason`,
        { code: 'discard_empty_reason', entry: { repository_path: norm } },
      );
    }
    if (!approvingTaskId || approvingTaskId.trim().length === 0) {
      throw new ManifestError(
        `approved_discard entry for ${norm} is missing an approving task id`,
        { code: 'discard_missing_task_id', entry: { repository_path: norm } },
      );
    }
  }

  return {
    repository_path: norm,
    blob_sha256: blobSha256,
    normalized_sha256: normalizedSha256,
    permalink,
    db_selection: db || null,
    disposition,
    rationale,
    approving_task_id: approvingTaskId || null,
  };
}

// ── Knowledge manifest generation ────────────────────────────────────────────

/**
 * Generate the complete knowledge manifest from NUL-delimited path bytes.
 *
 * - Derives the tracked knowledge set at runtime (no hard-coded count).
 * - Sorts entries deterministically by repository path.
 * - Enforces strict invariants: no duplicate paths, every entry resolved,
 *   ambiguous DB matches rejected.
 *
 * Returns the manifest object (not yet written).
 */
export function generateKnowledgeManifest(pathBytes, revision, dbSelection, opts = {}) {
  const allPaths = splitNulPaths(pathBytes);
  const knowledgePaths = allPaths
    .filter(isKnowledgePath)
    .filter((path) => !RETIRED_OPERATIONAL_PATHS.has(path));

  // Enforce no duplicate repository paths.
  const seen = new Set();
  for (const p of knowledgePaths) {
    if (seen.has(p)) {
      throw new ManifestError(`duplicate repository path in input: ${p}`, {
        code: 'duplicate_path',
        entry: { repository_path: p },
      });
    }
    seen.add(p);
  }

  // Deterministic ordering.
  knowledgePaths.sort();

  const entries = [];
  const seenPermalinks = new Map();
  for (const repoPath of knowledgePaths) {
    const entry = buildKnowledgeEntry(repoPath, revision, dbSelection, {
      git: opts.git,
      cwd: opts.cwd,
      seenPermalinks,
    });
    // Track permalinks to detect ambiguous matches (same permalink, different path).
    if (entry.permalink) {
      if (seenPermalinks.has(entry.permalink)) {
        const prev = seenPermalinks.get(entry.permalink);
        throw new ManifestError(
          `ambiguous permalink ${entry.permalink} matches multiple repository paths: ${prev} and ${repoPath}`,
          { code: 'ambiguous_permalink', entry: { permalink: entry.permalink } },
        );
      }
      seenPermalinks.set(entry.permalink, repoPath);
    }
    entries.push(entry);
  }

  return {
    schema: 'djinn-retirement-knowledge-manifest/v1',
    generated_from_revision: revision,
    knowledge_count: entries.length,
    entries,
  };
}

// ── DB guidance manifest generation ───────────────────────────────────────────

/**
 * Validate and load the hermetic DB-guidance fixture.
 *
 * Shape:
 *   {
 *     "schema": "djinn-retirement-db-guidance/v1",
 *     "records": [
 *       {
 *         "uuid": "...",
 *         "permalink": "...",
 *         "status": "active|archived|deprecated",
 *         "normalized_sha256": "<hex>",
 *         "classification": "preserve|archive|deprecate|rewrite",
 *         "disposition": "equivalent|db_supersedes_file|approved_discard",
 *         "rationale": "...",
 *         "superseded_by": "<permalink or uuid or null>",
 *         "supersedes": "<permalink or uuid or null>",
 *         "source_repository_path": "<optional .djinn path this reconciles>"
 *       }
 *     ]
 *   }
 */
export function loadDbGuidanceFixture(fixturePath) {
  if (!fixturePath) {
    return { schema: null, records: [] };
  }
  const abs = resolve(fixturePath);
  if (!existsSync(abs)) {
    throw new ManifestError(`DB guidance fixture not found: ${abs}`, {
      code: 'guidance_fixture_missing',
    });
  }
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(abs, 'utf8'));
  } catch (err) {
    throw new ManifestError(`DB guidance fixture is not valid JSON: ${err.message}`, {
      code: 'guidance_fixture_invalid_json',
    });
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new ManifestError('DB guidance fixture must be a JSON object', {
      code: 'guidance_fixture_shape',
    });
  }
  if (parsed.schema !== 'djinn-retirement-db-guidance/v1') {
    throw new ManifestError(
      `DB guidance fixture has unsupported schema: ${JSON.stringify(parsed.schema)}`,
      { code: 'guidance_fixture_schema' },
    );
  }
  if (!Array.isArray(parsed.records)) {
    throw new ManifestError('DB guidance fixture `records` must be an array', {
      code: 'guidance_fixture_shape',
    });
  }
  const records = [];
  const seenUuids = new Set();
  for (const rec of parsed.records) {
    if (!rec || typeof rec !== 'object' || Array.isArray(rec)) {
      throw new ManifestError('DB guidance record must be an object', {
        code: 'guidance_record_shape',
      });
    }
    if (typeof rec.uuid !== 'string' || rec.uuid.length === 0) {
      throw new ManifestError('DB guidance record is missing a non-empty uuid', {
        code: 'guidance_record_uuid',
      });
    }
    if (seenUuids.has(rec.uuid)) {
      throw new ManifestError(`duplicate DB guidance uuid: ${rec.uuid}`, {
        code: 'guidance_record_duplicate_uuid',
        entry: { uuid: rec.uuid },
      });
    }
    seenUuids.add(rec.uuid);
    if (typeof rec.permalink !== 'string' || rec.permalink.length === 0) {
      throw new ManifestError(`DB guidance record ${rec.uuid} is missing a non-empty permalink`, {
        code: 'guidance_record_permalink',
      });
    }
    const status = typeof rec.status === 'string' ? rec.status : 'active';
    if (!['active', 'archived', 'deprecated'].includes(status)) {
      throw new ManifestError(`DB guidance record ${rec.uuid} has invalid status: ${status}`, {
        code: 'guidance_record_status',
      });
    }
    if (typeof rec.normalized_sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(rec.normalized_sha256)) {
      throw new ManifestError(`DB guidance record ${rec.uuid} is missing a valid normalized_sha256`, {
        code: 'guidance_record_hash',
      });
    }
    const classification = typeof rec.classification === 'string' ? rec.classification : null;
    if (!['preserve', 'archive', 'deprecate', 'rewrite'].includes(classification)) {
      throw new ManifestError(
        `DB guidance record ${rec.uuid} is missing a valid classification`,
        { code: 'guidance_record_classification', entry: { uuid: rec.uuid } },
      );
    }
    const disposition = typeof rec.disposition === 'string' ? rec.disposition : null;
    if (!KNOWLEDGE_DISPOSITIONS.has(disposition)) {
      throw new ManifestError(
        `DB guidance record ${rec.uuid} is missing a valid disposition`,
        { code: 'guidance_record_disposition', entry: { uuid: rec.uuid } },
      );
    }
    if (typeof rec.rationale !== 'string' || rec.rationale.trim().length === 0) {
      throw new ManifestError(`DB guidance record ${rec.uuid} is missing a non-empty rationale`, {
        code: 'guidance_record_rationale',
      });
    }
    records.push({
      uuid: rec.uuid,
      permalink: rec.permalink,
      status,
      normalized_sha256: rec.normalized_sha256,
      classification,
      disposition,
      rationale: rec.rationale,
      superseded_by: typeof rec.superseded_by === 'string' ? rec.superseded_by : null,
      supersedes: typeof rec.supersedes === 'string' ? rec.supersedes : null,
      source_repository_path: typeof rec.source_repository_path === 'string' ? rec.source_repository_path : null,
    });
  }
  return { schema: parsed.schema, records };
}

/**
 * Generate the DB guidance manifest.
 *
 * Entries are sourced from the hermetic guidance fixture and sorted
 * deterministically by permalink. Supersession linkage fields are preserved
 * verbatim for the follow-up DB reconciliation task.
 */
export function generateDbGuidanceManifest(guidanceFixture) {
  const records = guidanceFixture.records.slice();
  records.sort((a, b) => {
    if (a.permalink === b.permalink) return a.uuid.localeCompare(b.uuid);
    return a.permalink.localeCompare(b.permalink);
  });
  return {
    schema: 'djinn-retirement-db-guidance-manifest/v1',
    record_count: records.length,
    entries: records,
  };
}

// ── Strict reconciliation guard ──────────────────────────────────────────────

/**
 * Strictly validate a generated knowledge manifest against invariants.
 *
 * Rejects:
 *   - ambiguous DB matches (a permalink resolving to >1 path)
 *   - duplicate repository paths
 *   - tracked/deletion count mismatch (entries.length !== expectedKnowledgeCount)
 *   - tracked/deletion set mismatch (entry paths !== expectedKnowledgeSet)
 *   - missing preserved identity (db_selection null on equivalent/db_supersedes)
 *   - empty discard reason or approving task id
 *   - unresolved entries (disposition not in the allowed set)
 *
 * Returns the manifest unchanged on success; throws ManifestError on failure.
 */
export function validateKnowledgeManifest(manifest, expected) {
  if (!manifest || typeof manifest !== 'object') {
    throw new ManifestError('manifest must be an object', { code: 'manifest_shape' });
  }
  const entries = Array.isArray(manifest.entries) ? manifest.entries : [];
  const byPath = new Map();
  for (const entry of entries) {
    if (!entry || typeof entry !== 'object') {
      throw new ManifestError('manifest entry must be an object', { code: 'entry_shape' });
    }
    const p = entry.repository_path;
    if (typeof p !== 'string' || p.length === 0) {
      throw new ManifestError('manifest entry is missing repository_path', {
        code: 'entry_missing_path',
      });
    }
    if (byPath.has(p)) {
      throw new ManifestError(`duplicate repository path in manifest: ${p}`, {
        code: 'duplicate_path',
        entry: { repository_path: p },
      });
    }
    byPath.set(p, entry);

    // Disposition must be exactly one allowed value.
    if (!KNOWLEDGE_DISPOSITIONS.has(entry.disposition)) {
      throw new ManifestError(
        `entry ${p} has invalid disposition: ${entry.disposition}`,
        { code: 'invalid_disposition', entry: { repository_path: p } },
      );
    }

    if (entry.disposition !== 'approved_discard' &&
        (typeof entry.rationale !== 'string' || entry.rationale.trim().length === 0)) {
      throw new ManifestError(`entry ${p} has an empty rationale`, {
        code: 'empty_rationale', entry: { repository_path: p },
      });
    }

    // equivalent / db_supersedes_file require a preserved DB identity.
    if (
      (entry.disposition === 'equivalent' || entry.disposition === 'db_supersedes_file') &&
      !entry.db_selection
    ) {
      throw new ManifestError(
        `entry ${p} has disposition ${entry.disposition} but no preserved DB identity`,
        { code: 'missing_preserved_identity', entry: { repository_path: p } },
      );
    }

    // approved_discard requires a non-empty reason and approving task id.
    if (entry.disposition === 'approved_discard') {
      if (typeof entry.rationale !== 'string' || entry.rationale.trim().length === 0) {
        throw new ManifestError(
          `approved_discard entry ${p} has an empty reason`,
          { code: 'discard_empty_reason', entry: { repository_path: p } },
        );
      }
      if (
        typeof entry.approving_task_id !== 'string' ||
        entry.approving_task_id.trim().length === 0
      ) {
        throw new ManifestError(
          `approved_discard entry ${p} is missing an approving task id`,
          { code: 'discard_missing_task_id', entry: { repository_path: p } },
        );
      }
    }

    // Hash fields must be present and well-formed.
    if (!/^[0-9a-f]{64}$/.test(entry.blob_sha256 || '')) {
      throw new ManifestError(`entry ${p} has an invalid blob_sha256`, {
        code: 'invalid_blob_hash',
        entry: { repository_path: p },
      });
    }
    if (!/^[0-9a-f]{64}$/.test(entry.normalized_sha256 || '')) {
      throw new ManifestError(`entry ${p} has an invalid normalized_sha256`, {
        code: 'invalid_normalized_hash',
        entry: { repository_path: p },
      });
    }
    if (typeof entry.permalink !== 'string' || entry.permalink.length === 0) {
      throw new ManifestError(`entry ${p} is missing permalink`, {
        code: 'entry_missing_permalink', entry: { repository_path: p },
      });
    }
    if (entry.db_selection !== null) {
      const db = entry.db_selection;
      if (!db || typeof db !== 'object' || typeof db.uuid !== 'string' || db.uuid.length === 0) {
        throw new ManifestError(`entry ${p} has an invalid DB identity`, {
          code: 'invalid_db_identity', entry: { repository_path: p },
        });
      }
      if (db.permalink !== entry.permalink) {
        throw new ManifestError(`entry ${p} DB permalink does not match its permalink`, {
          code: 'db_permalink_mismatch', entry: { repository_path: p },
        });
      }
      if (!['active', 'archived', 'deprecated'].includes(db.status)) {
        throw new ManifestError(`entry ${p} has an invalid DB status`, {
          code: 'invalid_db_status', entry: { repository_path: p },
        });
      }
      if (!/^[0-9a-f]{64}$/.test(db.normalized_sha256 || '')) {
        throw new ManifestError(`entry ${p} has an invalid DB normalized hash`, {
          code: 'invalid_db_hash', entry: { repository_path: p },
        });
      }
      if (entry.disposition === 'equivalent' && db.normalized_sha256 !== entry.normalized_sha256) {
        throw new ManifestError(`entry ${p} is equivalent but its normalized hashes differ`, {
          code: 'equivalent_hash_mismatch', entry: { repository_path: p },
        });
      }
      if (entry.disposition === 'db_supersedes_file' && db.normalized_sha256 === entry.normalized_sha256) {
        throw new ManifestError(`entry ${p} is superseded but its normalized hashes match`, {
          code: 'superseded_hash_match', entry: { repository_path: p },
        });
      }
    }
  }

  // Ambiguous permalink: a permalink that resolves to more than one repository
  // path is an ambiguous DB match and must be rejected.
  const permalinkPaths = new Map();
  for (const entry of entries) {
    if (typeof entry.permalink === 'string' && entry.permalink.length > 0) {
      if (permalinkPaths.has(entry.permalink)) {
        throw new ManifestError(
          `ambiguous permalink ${entry.permalink} resolves to multiple repository paths: ` +
            `${permalinkPaths.get(entry.permalink)} and ${entry.repository_path}`,
          { code: 'ambiguous_permalink', entry: { permalink: entry.permalink } },
        );
      }
      permalinkPaths.set(entry.permalink, entry.repository_path);
    }
  }

  // Count mismatch.
  if (expected && typeof expected.knowledgeCount === 'number') {
    if (entries.length !== expected.knowledgeCount) {
      throw new ManifestError(
        `tracked/deletion count mismatch: manifest has ${entries.length} entries but expected ${expected.knowledgeCount}`,
        { code: 'count_mismatch',
          entry: { manifest_count: entries.length, expected_count: expected.knowledgeCount } },
      );
    }
  }

  // Set mismatch.
  if (expected && expected.knowledgeSet instanceof Set) {
    const manifestSet = new Set(entries.map((e) => e.repository_path));
    for (const p of expected.knowledgeSet) {
      if (!manifestSet.has(p)) {
        throw new ManifestError(
          `tracked/deletion set mismatch: expected path ${p} is absent from the manifest`,
          { code: 'set_mismatch_missing', entry: { repository_path: p } },
        );
      }
    }
    for (const p of manifestSet) {
      if (!expected.knowledgeSet.has(p)) {
        throw new ManifestError(
          `tracked/deletion set mismatch: manifest path ${p} is not in the expected knowledge set`,
          { code: 'set_mismatch_extra', entry: { repository_path: p } },
        );
      }
    }
  }

  return manifest;
}

/**
 * Strictly validate a generated DB guidance manifest.
 *
 * Rejects missing guidance disposition, missing preserved identity, and
 * unresolved entries.
 */
export function validateDbGuidanceManifest(guidanceManifest) {
  if (!guidanceManifest || typeof guidanceManifest !== 'object') {
    throw new ManifestError('guidance manifest must be an object', { code: 'guidance_manifest_shape' });
  }
  const entries = Array.isArray(guidanceManifest.entries) ? guidanceManifest.entries : [];
  const seenUuids = new Set();
  for (const entry of entries) {
    if (!entry || typeof entry !== 'object') {
      throw new ManifestError('guidance manifest entry must be an object', { code: 'guidance_entry_shape' });
    }
    if (typeof entry.uuid !== 'string' || entry.uuid.length === 0) {
      throw new ManifestError('guidance entry is missing uuid', { code: 'guidance_entry_uuid' });
    }
    if (seenUuids.has(entry.uuid)) {
      throw new ManifestError(`duplicate guidance uuid: ${entry.uuid}`, {
        code: 'guidance_duplicate_uuid', entry: { uuid: entry.uuid },
      });
    }
    seenUuids.add(entry.uuid);
    if (!KNOWLEDGE_DISPOSITIONS.has(entry.disposition)) {
      throw new ManifestError(
        `guidance entry ${entry.uuid} has invalid disposition: ${entry.disposition}`,
        { code: 'guidance_invalid_disposition', entry: { uuid: entry.uuid } },
      );
    }
    if (typeof entry.rationale !== 'string' || entry.rationale.trim().length === 0) {
      throw new ManifestError(`guidance entry ${entry.uuid} has an empty rationale`, {
        code: 'guidance_empty_rationale', entry: { uuid: entry.uuid },
      });
    }
    if (typeof entry.permalink !== 'string' || entry.permalink.length === 0) {
      throw new ManifestError(`guidance entry ${entry.uuid} is missing permalink`, {
        code: 'guidance_entry_permalink', entry: { uuid: entry.uuid },
      });
    }
    if (!/^[0-9a-f]{64}$/.test(entry.normalized_sha256 || '')) {
      throw new ManifestError(`guidance entry ${entry.uuid} has invalid normalized_sha256`, {
        code: 'guidance_invalid_hash', entry: { uuid: entry.uuid },
      });
    }
    if (!['active', 'archived', 'deprecated'].includes(entry.status)) {
      throw new ManifestError(`guidance entry ${entry.uuid} has invalid status`, {
        code: 'guidance_invalid_status', entry: { uuid: entry.uuid },
      });
    }
    if (!['preserve', 'archive', 'deprecate', 'rewrite'].includes(entry.classification)) {
      throw new ManifestError(`guidance entry ${entry.uuid} has invalid classification`, {
        code: 'guidance_invalid_classification', entry: { uuid: entry.uuid },
      });
    }
  }
  return guidanceManifest;
}

/** Validate the durable deletion ledger and enforce the post-cutover state. */
export function validateRetirementCutover(currentPathBytes, ledger, guidanceFixture, opts = {}) {
  if (!ledger || ledger.schema !== 'djinn-retirement-deletion-ledger/v1') {
    throw new ManifestError('deletion ledger has an unsupported schema', { code: 'ledger_schema' });
  }
  const revision = ledger.generated_from_revision;
  if (typeof revision !== 'string' || !/^[0-9a-f]{40,64}$/.test(revision)) {
    throw new ManifestError('deletion ledger is missing an immutable source revision', {
      code: 'ledger_revision',
    });
  }
  let sourcePathBytes;
  try {
    sourcePathBytes = execFileSync(opts.git || 'git', [
      'ls-tree', '-rz', '--name-only', revision, '--', '.djinn',
    ], { cwd: opts.cwd, maxBuffer: 64 * 1024 * 1024 });
  } catch (err) {
    throw new ManifestError(`failed to inspect deletion source revision ${revision}: ${err.message}`, {
      code: 'ledger_source_revision',
    });
  }
  const sourceKnowledge = new Set(
    splitNulPaths(sourcePathBytes).filter(isKnowledgePath)
      .filter((path) => path !== '.djinn/.gitignore' && path !== '.djinn/skills.json'),
  );
  validateKnowledgeManifest(ledger, {
    knowledgeCount: sourceKnowledge.size,
    knowledgeSet: sourceKnowledge,
  });
  if (ledger.knowledge_count !== ledger.entries.length) {
    throw new ManifestError('deletion ledger knowledge_count does not match its entries', {
      code: 'ledger_count_mismatch',
    });
  }
  for (const entry of ledger.entries) {
    if (!isKnowledgePath(entry.repository_path)) {
      throw new ManifestError(`ledger path is not classified knowledge: ${entry.repository_path}`, {
        code: 'ledger_non_knowledge_path', entry: { repository_path: entry.repository_path },
      });
    }
    const blob = readCommittedBlob(entry.repository_path, revision, opts);
    if (sha256Hex(blob) !== entry.blob_sha256 ||
        sha256Hex(normalizeContent(blob)) !== entry.normalized_sha256) {
      throw new ManifestError(`ledger hashes do not match source blob: ${entry.repository_path}`, {
        code: 'ledger_source_hash_mismatch', entry: { repository_path: entry.repository_path },
      });
    }
    if (detectPermalink(entry.repository_path) !== entry.permalink) {
      throw new ManifestError(`ledger permalink does not match source path: ${entry.repository_path}`, {
        code: 'ledger_permalink_mismatch', entry: { repository_path: entry.repository_path },
      });
    }
  }

  const guidanceManifest = generateDbGuidanceManifest(guidanceFixture);
  validateDbGuidanceManifest(guidanceManifest);
  const guidanceByPath = new Map();
  for (const guidance of guidanceManifest.entries) {
    if (typeof guidance.source_repository_path !== 'string' ||
        guidanceByPath.has(guidance.source_repository_path)) {
      throw new ManifestError('guidance source paths must be present and unique', {
        code: 'guidance_source_path', entry: { uuid: guidance.uuid },
      });
    }
    guidanceByPath.set(guidance.source_repository_path, guidance);
  }
  for (const entry of ledger.entries) {
    const guidance = guidanceByPath.get(entry.repository_path);
    if (!guidance) {
      throw new ManifestError(`ledger entry has no DB guidance: ${entry.repository_path}`, {
        code: 'ledger_missing_guidance', entry: { repository_path: entry.repository_path },
      });
    }
    if (entry.db_selection) {
      const db = entry.db_selection;
      if (guidance.uuid !== db.uuid || guidance.permalink !== db.permalink ||
          guidance.status !== db.status || guidance.normalized_sha256 !== db.normalized_sha256 ||
          guidance.disposition !== entry.disposition) {
        throw new ManifestError(`ledger DB identity disagrees with guidance: ${entry.repository_path}`, {
          code: 'ledger_guidance_mismatch', entry: { repository_path: entry.repository_path },
        });
      }
    }
  }
  if (guidanceByPath.size !== ledger.entries.length) {
    throw new ManifestError('guidance/deletion count mismatch', {
      code: 'guidance_deletion_count_mismatch',
    });
  }

  const currentPaths = splitNulPaths(
    Buffer.isBuffer(currentPathBytes) ? currentPathBytes : Buffer.from(currentPathBytes || '', 'binary'),
  );
  const currentSet = new Set(currentPaths);
  for (const retiredPath of RETIRED_OPERATIONAL_PATHS) {
    if (currentSet.has(retiredPath)) {
      throw new ManifestError(`retired operational path was reintroduced: ${retiredPath}`, {
        code: 'retired_path_reintroduced', entry: { repository_path: retiredPath },
      });
    }
  }
  assertNoProjectLocalDjinnSurface(currentPaths.map((path) => ({ path })));
  assertNoProjectLocalDjinnSurface(discoverProjectLocalDjinnSurfaces(currentPaths, opts));
  const currentKnowledge = currentPaths
    .filter(isKnowledgePath)
    .filter((path) => !RETIRED_OPERATIONAL_PATHS.has(path));
  if (currentKnowledge.length > 0) {
    throw new ManifestError(`tracked project-local knowledge was reintroduced: ${currentKnowledge[0]}`, {
      code: 'knowledge_reintroduced', entry: { repository_path: currentKnowledge[0] },
    });
  }
  const preservedOperationalPaths = [...NON_KNOWLEDGE_TRACKED]
    .filter((path) => !RETIRED_OPERATIONAL_PATHS.has(path));
  for (const operationalPath of preservedOperationalPaths) {
    if (!currentSet.has(operationalPath)) {
      throw new ManifestError(`required operational path is missing: ${operationalPath}`, {
        code: 'operational_path_missing', entry: { repository_path: operationalPath },
      });
    }
    const sourceBlob = readCommittedBlob(operationalPath, revision, opts);
    const currentBlob = readCommittedBlob(operationalPath, opts.currentRevision || 'HEAD', opts);
    if (!sourceBlob.equals(currentBlob)) {
      throw new ManifestError(`operational path changed during cutover: ${operationalPath}`, {
        code: 'operational_path_changed', entry: { repository_path: operationalPath },
      });
    }
  }
  return { ledger, guidanceManifest };
}

// ── Top-level generation ─────────────────────────────────────────────────────

/**
 * Generate both manifests and write them under `outputDir`.
 *
 * Inputs:
 *   - pathBytes: NUL-delimited `git ls-files -z` output (Buffer or string).
 *   - revision: explicit git revision for committed blob reads (default HEAD).
 *   - dbSelectionFixturePath: path to the hermetic DB-selection fixture JSON.
 *   - dbGuidanceFixturePath: path to the hermetic DB-guidance fixture JSON.
 *   - outputDir: directory to write the two JSON manifests.
 *
 * Returns { knowledgeManifest, dbGuidanceManifest }.
 */
export function generateAll(pathBytes, opts = {}) {
  const revision = opts.revision || 'HEAD';
  const outputDir = opts.outputDir || DEFAULT_OUTPUT_DIR;
  const dbSelection = loadDbSelectionFixture(opts.dbSelectionFixturePath);
  const dbGuidance = loadDbGuidanceFixture(opts.dbGuidanceFixturePath);

  const knowledgeManifest = generateKnowledgeManifest(
    Buffer.isBuffer(pathBytes) ? pathBytes : Buffer.from(pathBytes || '', 'binary'),
    revision,
    dbSelection,
    { git: opts.git, cwd: opts.cwd },
  );

  // Compute expected knowledge set/count for strict validation.
  const allPaths = splitNulPaths(Buffer.isBuffer(pathBytes) ? pathBytes : Buffer.from(pathBytes || '', 'binary'));
  const expectedKnowledgeSet = new Set(
    allPaths
      .filter(isKnowledgePath)
      .filter((path) => !RETIRED_OPERATIONAL_PATHS.has(path)),
  );
  validateKnowledgeManifest(knowledgeManifest, {
    knowledgeCount: expectedKnowledgeSet.size,
    knowledgeSet: expectedKnowledgeSet,
  });

  const dbGuidanceManifest = generateDbGuidanceManifest(dbGuidance);
  validateDbGuidanceManifest(dbGuidanceManifest);

  // Write outputs (deterministic: 2-space indent, sorted keys, trailing newline).
  const absOut = resolve(outputDir);
  mkdirSync(absOut, { recursive: true });
  writeFileSync(
    resolve(absOut, 'knowledge-manifest.json'),
    `${JSON.stringify(knowledgeManifest, null, 2)}\n`,
  );
  writeFileSync(
    resolve(absOut, 'db-guidance-manifest.json'),
    `${JSON.stringify(dbGuidanceManifest, null, 2)}\n`,
  );

  return { knowledgeManifest, dbGuidanceManifest };
}

// ── CLI ──────────────────────────────────────────────────────────────────────

function parseCliArgs(argv) {
  const { values, positionals } = parseArgs({
    args: argv,
    options: {
      revision: { type: 'string', short: 'r' },
      'db-selection': { type: 'string' },
      'db-guidance': { type: 'string' },
      'deletion-ledger': { type: 'string' },
      'output-dir': { type: 'string', short: 'o' },
      'paths-file': { type: 'string' },
      help: { type: 'boolean', short: 'h' },
    },
    allowNegative: true,
    strict: false,
  });
  return { values, positionals };
}

function main(argv) {
  const { values } = parseCliArgs(argv);
  if (values.help) {
    process.stdout.write(
      [
        'usage: djinn-retirement-manifest.mjs [options]',
        '',
        'Reads NUL-delimited git ls-files paths from stdin (or --paths-file) and',
        'writes target/djinn-retirement/{knowledge,db-guidance}-manifest.json.',
        '',
        'options:',
        '  -r, --revision <rev>        git revision for committed blob reads (default HEAD)',
        '      --db-selection <path>   hermetic DB-selection fixture JSON',
        '      --db-guidance <path>    hermetic DB-guidance fixture JSON',
        '      --deletion-ledger <path> validate durable ledger and post-cutover state',
        '  -o, --output-dir <dir>      output directory (default target/djinn-retirement)',
        '      --paths-file <path>     read NUL-delimited paths from a file instead of stdin',
        '  -h, --help                  show this help',
        '',
      ].join('\n'),
    );
    return;
  }

  let pathBytes;
  if (values['paths-file']) {
    pathBytes = readFileSync(values['paths-file']);
  } else {
    // Pipes can return EAGAIN with Node's one-shot readFileSync. Read raw
    // NUL-delimited stdin incrementally so the shell guard is reliable.
    const chunks = [];
    // stdin is nonblocking under the Node test runner and some CI shells.
    if (process.stdin._handle?.setBlocking) process.stdin._handle.setBlocking(true);
    const buffer = Buffer.alloc(64 * 1024);
    while (true) {
      let count;
      try {
        count = readSync(process.stdin.fd, buffer, 0, buffer.length, null);
      } catch (err) {
        if (err.code === 'EAGAIN') continue;
        if (err.code === 'EOF') break;
        throw err;
      }
      if (count === 0) break;
      chunks.push(Buffer.from(buffer.subarray(0, count)));
    }
    pathBytes = Buffer.concat(chunks);
  }

  if (values['deletion-ledger']) {
    let ledger;
    try {
      ledger = JSON.parse(readFileSync(values['deletion-ledger'], 'utf8'));
    } catch (err) {
      throw new ManifestError(`deletion ledger is not valid JSON: ${err.message}`, {
        code: 'ledger_invalid_json',
      });
    }
    const guidance = loadDbGuidanceFixture(values['db-guidance']);
    const result = validateRetirementCutover(pathBytes, ledger, guidance);
    process.stderr.write(
      `validated ${result.ledger.knowledge_count} durable deletion entries and ` +
        `${result.guidanceManifest.record_count} guidance entries; tracked knowledge set is empty\n`,
    );
    return;
  }

  const result = generateAll(pathBytes, {
    revision: values.revision,
    dbSelectionFixturePath: values['db-selection'],
    dbGuidanceFixturePath: values['db-guidance'],
    outputDir: values['output-dir'],
  });

  const outDir = values['output-dir'] || DEFAULT_OUTPUT_DIR;
  process.stderr.write(
    `wrote ${result.knowledgeManifest.knowledge_count} knowledge entries and ` +
      `${result.dbGuidanceManifest.record_count} guidance entries to ${resolve(outDir)}\n`,
  );
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    main(process.argv.slice(2));
  } catch (err) {
    if (err instanceof ManifestError) {
      process.stderr.write(`ERROR: ${err.message}\n`);
      process.exitCode = 1;
    } else {
      throw err;
    }
  }
}
