// Shared source-text helpers for the JavaScript CI guards.
//
// WHY THIS FILE EXISTS
//
// The `scripts/ci-*.test.mjs` guards assert on workflow YAML as raw text.
// Comments are text, so every one of those assertions has a defect, and the
// direction decides the damage:
//
//   * a BAN false-POSITIVES on prose. `assertQaJobHasNoLiveDependencies`
//     forbids /\bkind\b/i in the qa-smoke job; the English word "kind" in a
//     `#` comment ("this kind of failure") reds the build with no live
//     dependency added anywhere.
//   * a PRESENCE assertion false-NEGATIVES, and this is the silent direction.
//     Delete the `psql … CREATE DATABASE djinn_test_template` step, leave
//     `# CREATE DATABASE djinn_test_template happens in the compose file now`,
//     and the guard stays green while the job no longer creates its database.
//
// Both shapes have already shipped in this repo in other languages:
// `1j64`'s teardown-trap guard matched a comment mentioning `trap … EXIT` and
// stayed green when the real `trap` was deleted; `0vku`'s CI guard passed on a
// commented-out arming call, the exact mutation its acceptance criterion
// named. The Rust-side answer lives in `scripts/lib/rust-source-scan.awk` and
// `server/tests/task_run_resize_kind.rs` (`code_lines` / `rust_code` /
// `script_code`, PR #2871). This is the same rule for YAML and shell.
//
// THE ONE INVARIANT: a trailing comment must not launder the code in front of
// it. `run: make test  # no kubectl needed` still runs `make test`, so the
// text before the `#` survives. A helper that dropped the whole line would let
// any violation be hidden by typing a comment after it.

/**
 * Remove `#` comments from YAML or shell text.
 *
 * A `#` starts a comment only at the start of a line or after whitespace —
 * that is YAML's own rule, and it is also close enough for the shell inside a
 * `run: |` block. So `postgres://user:pw@host/db#frag` and `$#` keep their
 * `#`, while `save-if: false  # restore only` loses the tail and keeps the
 * key.
 *
 * Quoted spans are honoured, so a `#` inside `"a # b"` is data, not a comment.
 * Line count is preserved: these guards use `/^ {10}key:/m` anchors and
 * report line numbers, and renumbering them would be its own defect.
 *
 * @param {string} text
 * @returns {string}
 */
export function scriptCode(text) {
  return text
    .split('\n')
    .map((line) => stripHashComment(line))
    .join('\n');
}

function stripHashComment(line) {
  let quote = null;
  for (let i = 0; i < line.length; i += 1) {
    const char = line[i];
    if (quote) {
      // YAML single-quoted scalars escape a quote by doubling it; double-quoted
      // ones use a backslash. Either way the span continues.
      if (char === '\\' && quote === '"') {
        i += 1;
        continue;
      }
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      continue;
    }
    if (char === '#' && (i === 0 || /\s/.test(line[i - 1]))) {
      return line.slice(0, i);
    }
  }
  return line;
}

/**
 * Remove `//` line comments and `/* *\/` block comments from JavaScript,
 * TypeScript or Rust text, honouring string literals so a `//` inside
 * `"https://x"` neither opens a comment nor hides one.
 *
 * Same invariant as {@link scriptCode}: the code in front of a trailing
 * comment survives, and the line count is preserved.
 *
 * @param {string} text
 * @returns {string}
 */
export function slashCode(text) {
  let out = '';
  let inBlock = false;
  let quote = null;
  for (let i = 0; i < text.length; i += 1) {
    const char = text[i];
    const next = text[i + 1];
    if (inBlock) {
      if (char === '\n') out += '\n';
      else if (char === '*' && next === '/') {
        inBlock = false;
        i += 1;
      }
      continue;
    }
    if (quote) {
      out += char;
      if (char === '\\') {
        out += next ?? '';
        i += 1;
      } else if (char === quote || char === '\n') {
        quote = null;
      }
      continue;
    }
    if (char === '"' || char === "'" || char === '`') {
      quote = char;
      out += char;
      continue;
    }
    if (char === '/' && next === '/') {
      const end = text.indexOf('\n', i);
      if (end === -1) break;
      i = end - 1;
      continue;
    }
    if (char === '/' && next === '*') {
      inBlock = true;
      i += 1;
      continue;
    }
    out += char;
  }
  return out;
}
