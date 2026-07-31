// Self-test for scripts/lib/source-text.mjs.
//
// Every case is a PAIR. For each thing the stripper must ignore there is an
// adjacent thing it must still keep, because "strips comments" and "strips
// everything" are indistinguishable from a green run — and a guard whose
// stripper returned the empty string would report OK forever.
//
// The same reasoning, and the same two traps, as scripts/test-rust-source-scan.sh:
//
//   * A PRESENCE assertion and a BAN assertion need DIFFERENT mutations.
//     Swapping a required token for another valid one exercises the presence
//     arm and proves nothing about the ban. To test a ban you must ADD the
//     banned token while LEAVING the required one in place.
//   * A trailing comment must not launder the code in front of it.

import assert from 'node:assert/strict';
import test from 'node:test';

import { scriptCode, slashCode } from './lib/source-text.mjs';

test('scriptCode drops comment prose but keeps the code beside it', () => {
  const yaml = [
    '      # kubectl is deliberately absent from this job',
    '      - name: Run tests',
    '        run: make test  # no kubectl needed here either',
    '        env:',
    '          KEY: value # trailing',
  ].join('\n');
  const code = scriptCode(yaml);

  // BAN direction: the word only ever appears in prose, so the ban must not
  // fire. This is the /\bkind\b/i and /https?:\/\// shape that reds the
  // qa-smoke job on an English sentence.
  assert.doesNotMatch(code, /kubectl/, 'a comment mentioning kubectl is not a dependency');

  // The load-bearing pair: if the stripper dropped whole lines, `make test`
  // and `KEY: value` would have gone with the comments.
  assert.match(code, /run: make test/, 'code before a trailing comment survives');
  assert.match(code, /KEY: value/, 'a trailing comment does not launder the key');
  assert.doesNotMatch(code, /trailing/, 'the trailing comment itself is gone');
});

test('scriptCode adds the ban back when the token is real', () => {
  // The correct mutation for a BAN: ADD the banned token as real code while
  // leaving everything else in place. Replacing a required token instead would
  // exercise the presence arm and prove nothing here.
  const yaml = [
    '      # kubectl is deliberately absent from this job',
    '      - name: Run tests',
    '        run: kubectl apply -f manifest.yaml',
  ].join('\n');
  assert.match(scriptCode(yaml), /kubectl apply/, 'a real invocation is still visible');
});

test('scriptCode does not treat a # inside a value or a quote as a comment', () => {
  const yaml = [
    '        run: psql postgres://user:pw@127.0.0.1:5433/db#frag',
    '        run: echo "a # b" && kubectl version',
    "        run: echo 'c # d'",
    '        run: echo "$#"',
  ].join('\n');
  const code = scriptCode(yaml);
  assert.match(code, /db#frag/, 'a # not preceded by whitespace is not a comment');
  assert.match(code, /a # b/, 'a # inside a double-quoted scalar is data');
  assert.match(code, /c # d/, 'a # inside a single-quoted scalar is data');
  assert.match(code, /kubectl version/, 'a quoted # does not swallow the rest of the line');
  assert.match(code, /\$#/, 'a shell $# is not a comment');
});

test('scriptCode preserves the line count', () => {
  // The guards anchor on `/^ {10}key:/m` and report line numbers. Renumbering
  // them while fixing comment blindness would be its own defect.
  const yaml = ['# a', 'b: 1', '', '# c', 'd: 2'].join('\n');
  assert.equal(scriptCode(yaml).split('\n').length, yaml.split('\n').length);
});

test('scriptCode makes a commented-out step stop satisfying a presence check', () => {
  // The 0vku shape: a CI guard passed on a commented-out call, the exact
  // mutation its acceptance criterion named.
  const deleted = '        # run: psql -c "CREATE DATABASE djinn_test_template"';
  const live = '        run: psql -c "CREATE DATABASE djinn_test_template"';
  assert.doesNotMatch(scriptCode(deleted), /CREATE DATABASE djinn_test_template/);
  assert.match(scriptCode(live), /CREATE DATABASE djinn_test_template/);
});

test('slashCode strips // and /* */ without eating strings', () => {
  const js = [
    '// banned_call() in a comment',
    'const url = "https://example.com"; banned_call();',
    'const t = `a // b`;',
    '/* banned_call() in a block */',
    'banned_call(); // trailing banned_call() mention',
  ].join('\n');
  const code = slashCode(js);

  // A naive `line.replace(/\/\/.*/, '')` truncates line 2 at `https:` and
  // silently drops a real violation — a FALSE NEGATIVE created by fixing a
  // false positive.
  assert.match(code, /banned_call\(\);\n/, 'the real call after a URL survives');
  assert.match(code, /a \/\/ b/, 'a // inside a template literal is data');
  // Line 5 carries one real call AND one mention of the same token in its own
  // trailing comment: the count proves the stripper kept the code and dropped
  // the prose, rather than dropping or keeping the whole line.
  assert.equal((code.match(/banned_call\(\)/g) ?? []).length, 2,
    'only the two real calls remain; the three comment mentions are gone');
  assert.equal(slashCode(js).split('\n').length, js.split('\n').length,
    'line count is preserved');
});
