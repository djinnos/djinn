#!/usr/bin/env node
/**
 * Deterministic hermetic-fixture generator for the retirement manifest guard.
 *
 * Produces two committed fixtures under `scripts/fixtures/djinn-retirement/`:
 *
 *   - db-selection.json   — one synthetic DB-selection record per tracked
 *                            `.djinn` knowledge permalink, with a
 *                            normalized_sha256 that matches the committed
 *                            file's normalized content hash at HEAD so the
 *                            guard's happy path resolves to `equivalent`.
 *   - db-guidance.json    — one synthetic DB-guidance record per affected
 *                            permalink, carrying classification/disposition,
 *                            rationale, status, hashes, and supersession
 *                            linkage fields the follow-up DB task needs.
 *
 * The fixtures are SYNTHETIC and HERMETIC: the uuids are deterministic sha256
 * derivatives of the permalink, not real production DB uuids. They exist so
 * the strict guard can run against the live repository HEAD without production
 * credentials. The real DB reconciliation task (kvuf) will replace these with
 * actual DB-record selections.
 *
 * Usage:
 *   node scripts/fixtures/djinn-retirement/generate.mjs [--output-dir <dir>] [--revision HEAD]
 *
 * Default output dir is the fixture directory next to this script.
 */
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { writeFileSync, readFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import {
  normalizeContent,
  sha256Hex,
  splitNulPaths,
  isKnowledgePath,
  detectPermalink,
  readCommittedBlob,
} from '../../djinn-retirement-manifest.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const DEFAULT_OUTPUT_DIR = __dirname;

function deterministicUuid(prefix, permalink) {
  return `${prefix}-${createHash('sha256').update(permalink).digest('hex').slice(0, 24)}`;
}

function listKnowledgePaths(revision, { cwd } = {}) {
  const bytes = execFileSync('git', ['ls-files', '-z', '.djinn/*'], {
    cwd,
    maxBuffer: 64 * 1024 * 1024,
  });
  return splitNulPaths(bytes).filter(isKnowledgePath).sort();
}

function buildSelectionFixture(revision, { cwd } = {}) {
  const paths = listKnowledgePaths(revision, { cwd });
  const records = {};
  for (const repoPath of paths) {
    const permalink = detectPermalink(repoPath);
    if (!permalink) continue; // non-markdown knowledge has no permalink
    const blob = readCommittedBlob(repoPath, revision, { cwd });
    const normHash = sha256Hex(normalizeContent(blob));
    records[permalink] = {
      uuid: deterministicUuid('retire', permalink),
      permalink,
      status: 'active',
      normalized_sha256: normHash,
      confidence: 1.0,
    };
  }
  return {
    schema: 'djinn-retirement-db-selection/v1',
    generated_from_revision: revision,
    record_count: Object.keys(records).length,
    records,
  };
}

function buildGuidanceFixture(revision, { cwd } = {}) {
  const paths = listKnowledgePaths(revision, { cwd });
  const records = [];
  for (const repoPath of paths) {
    const permalink = detectPermalink(repoPath);
    if (!permalink) continue;
    const blob = readCommittedBlob(repoPath, revision, { cwd });
    const normHash = sha256Hex(normalizeContent(blob));
    const uuid = deterministicUuid('retire', permalink);
    records.push({
      uuid,
      permalink,
      status: 'active',
      normalized_sha256: normHash,
      classification: 'preserve',
      disposition: 'equivalent',
      rationale: 'Hermetic fixture: DB record preserves the knowledge with a matching normalized-content hash.',
      superseded_by: null,
      supersedes: null,
      source_repository_path: repoPath,
    });
  }
  // Deterministic ordering by permalink.
  records.sort((a, b) => a.permalink.localeCompare(b.permalink));
  return {
    schema: 'djinn-retirement-db-guidance/v1',
    generated_from_revision: revision,
    record_count: records.length,
    records,
  };
}

function main(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      revision: { type: 'string', short: 'r', default: 'HEAD' },
      'output-dir': { type: 'string', short: 'o', default: DEFAULT_OUTPUT_DIR },
      help: { type: 'boolean', short: 'h' },
    },
    allowNegative: true,
    strict: false,
  });
  if (values.help) {
    process.stdout.write(
      [
        'usage: generate.mjs [-r|--revision HEAD] [-o|--output-dir <dir>]',
        '',
        'Regenerates the hermetic db-selection.json and db-guidance.json fixtures',
        'from the tracked .djinn knowledge set at the given revision.',
        '',
      ].join('\n'),
    );
    return;
  }
  const revision = values.revision || 'HEAD';
  const outDir = resolve(values['output-dir'] || DEFAULT_OUTPUT_DIR);
  mkdirSync(outDir, { recursive: true });

  const selection = buildSelectionFixture(revision);
  const guidance = buildGuidanceFixture(revision);

  writeFileSync(
    resolve(outDir, 'db-selection.json'),
    `${JSON.stringify(selection, null, 2)}\n`,
  );
  writeFileSync(
    resolve(outDir, 'db-guidance.json'),
    `${JSON.stringify(guidance, null, 2)}\n`,
  );

  process.stderr.write(
    `wrote ${selection.record_count} selection records and ${guidance.record_count} guidance records to ${outDir}\n`,
  );
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main(process.argv.slice(2));
}

export { buildSelectionFixture, buildGuidanceFixture, deterministicUuid };
