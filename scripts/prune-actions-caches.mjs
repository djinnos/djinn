#!/usr/bin/env node
/**
 * Reclaim GitHub Actions cache budget.
 *
 * GitHub caps a repository at 10 GB and evicts LRU across the WHOLE repo, so
 * dead entries do not merely sit there — they push out the live ones. Two
 * mechanisms accumulate dead entries here, and neither self-corrects:
 *
 *  1. Swatinem/rust-cache never deletes the entry it supersedes. Its key ends
 *     in a hash of Cargo.lock + every workspace Cargo.toml, so every dependency
 *     bump or manifest edit mints a NEW key and strands the old one forever.
 *     Restores go through a prefix match, so only the newest entry per family
 *     is ever read; the rest are pure ballast at ~1.6 GB (server-test) and
 *     ~0.8 GB (server-aarch64-check) apiece.
 *
 *  2. Caches written from an ephemeral ref outlive the ref. A cache is scoped
 *     to the ref that wrote it, so once a PR merges or a merge-queue branch is
 *     deleted, its entries can never be read by anything again.
 *
 * Measured 2026-07-27 before this script existed: 26 entries, 10.15 GB against
 * a 10 GB cap — actively evicting — of which 2.7 GB was eleven byte-identical
 * pnpm entries on eleven dead refs and 2.4 GB was superseded rust entries.
 *
 * Usage:
 *   node scripts/prune-actions-caches.mjs [--dry-run] [--keep N] [--repo O/R]
 *
 * Requires the `gh` CLI authenticated with `actions: write` on the repo.
 */
import { execFileSync } from 'node:child_process';

const args = process.argv.slice(2);
const DRY_RUN = args.includes('--dry-run');
const KEEP = Number(valueOf('--keep') ?? 2);
const REPO = valueOf('--repo') ?? process.env.GITHUB_REPOSITORY;

function valueOf(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}

if (!REPO) {
  console.error('prune-actions-caches: pass --repo OWNER/NAME or set GITHUB_REPOSITORY');
  process.exit(2);
}
if (!Number.isInteger(KEEP) || KEEP < 1) {
  console.error(`prune-actions-caches: --keep must be a positive integer, got ${KEEP}`);
  process.exit(2);
}

function gh(endpoint, extra = []) {
  return execFileSync('gh', ['api', endpoint, ...extra], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
}

/**
 * All cache entries, following pagination. `--paginate` on a JSON object
 * endpoint emits one JSON document per page rather than a merged array, so
 * concatenate the per-page `actions_caches` arrays.
 */
function listCaches() {
  const raw = gh(`/repos/${REPO}/actions/caches?per_page=100`, ['--paginate', '--slurp']);
  return JSON.parse(raw).flatMap((page) => page.actions_caches ?? []);
}

/**
 * Group key for a rust-cache entry: the key with its trailing
 * `-<env-hash>-<lock-hash>` removed, so `v0-rust-server-test-Linux-x64-6e4c1e9c-fcf0d190`
 * and `…-6e4c1e9c-d7b4ddff` land in one family.
 *
 * Both hashes are stripped, not just the lock hash. The env hash changes when
 * rustc or any cargo- or rust-prefixed env var changes, and rust-cache's
 * restore prefix includes it — so entries under a retired env hash are as
 * unreadable
 * as entries under a retired lock hash, and must age out the same way. Keying
 * the family on the env hash would pin two stale entries per retired hash
 * forever.
 */
function rustFamily(key) {
  const match = /^(v\d+-rust-.+?)-[0-9a-f]{6,}-[0-9a-f]{6,}$/.exec(key);
  return match ? match[1] : null;
}

const PR_REF = /^refs\/pull\/(\d+)\/merge$/;
const QUEUE_REF = /^refs\/heads\/gh-readonly-queue\//;

const prStateCache = new Map();
function pullRequestIsOpen(number) {
  if (!prStateCache.has(number)) {
    let open = true; // fail safe: never delete on an unreadable state
    try {
      open = JSON.parse(gh(`/repos/${REPO}/pulls/${number}`)).state === 'open';
    } catch (error) {
      console.warn(`  ! could not read PR #${number} state, keeping its caches: ${error.message}`);
    }
    prStateCache.set(number, open);
  }
  return prStateCache.get(number);
}

const caches = listCaches();
const mb = (bytes) => Math.round(bytes / 1048576);
const totalBefore = caches.reduce((sum, entry) => sum + entry.size_in_bytes, 0);
console.log(`${caches.length} entries, ${(totalBefore / 1073741824).toFixed(2)} GB\n`);

/** @type {{entry: object, reason: string}[]} */
const doomed = [];

// ── Dead refs ───────────────────────────────────────────────────────────────
// A merge-queue branch is deleted the moment the queue concludes, and a merged
// or closed PR's merge ref stops being checked out. Neither can ever serve a
// restore again, whatever family it belongs to.
for (const entry of caches) {
  if (QUEUE_REF.test(entry.ref)) {
    doomed.push({ entry, reason: 'merge-queue ref no longer exists' });
    continue;
  }
  const pr = PR_REF.exec(entry.ref);
  if (pr && !pullRequestIsOpen(Number(pr[1]))) {
    doomed.push({ entry, reason: `PR #${pr[1]} is closed` });
  }
}

// ── Superseded rust entries ─────────────────────────────────────────────────
const alreadyDoomed = new Set(doomed.map(({ entry }) => entry.id));
const families = new Map();
for (const entry of caches) {
  if (alreadyDoomed.has(entry.id)) continue;
  const family = rustFamily(entry.key);
  if (!family) continue;
  if (!families.has(family)) families.set(family, []);
  families.get(family).push(entry);
}
for (const [family, entries] of families) {
  entries.sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
  for (const entry of entries.slice(KEEP)) {
    doomed.push({ entry, reason: `superseded in ${family} (keeping newest ${KEEP})` });
  }
}

if (doomed.length === 0) {
  console.log('nothing to prune');
  process.exit(0);
}

const reclaimed = doomed.reduce((sum, { entry }) => sum + entry.size_in_bytes, 0);
for (const { entry, reason } of doomed) {
  console.log(`${DRY_RUN ? 'would delete' : 'deleting'} ${mb(entry.size_in_bytes)} MB  ${entry.key}`);
  console.log(`    ref=${entry.ref}  ${reason}`);
  if (DRY_RUN) continue;
  try {
    gh(`/repos/${REPO}/actions/caches/${entry.id}`, ['-X', 'DELETE']);
  } catch (error) {
    // A concurrent run, or GitHub's own LRU, may have removed it already.
    console.warn(`    ! delete failed: ${error.message}`);
  }
}

const verb = DRY_RUN ? 'would reclaim' : 'reclaimed';
console.log(`\n${verb} ${(reclaimed / 1073741824).toFixed(2)} GB across ${doomed.length} entries`);
if (process.env.GITHUB_STEP_SUMMARY) {
  console.log(`::notice::cache-prune ${verb} ${(reclaimed / 1073741824).toFixed(2)} GB across ${doomed.length} entries`);
}
