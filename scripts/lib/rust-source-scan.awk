# Shared source-text scanner for Rust guards.
#
# WHY THIS FILE EXISTS
#
# Four separate guards were defeated in a single day by two defects, and each
# guard had written its own half-answer to them:
#
#   (a) COMMENT BLINDNESS. Matching raw source text conflates code with prose.
#       It bites in BOTH directions and the direction decides the damage:
#         * a BAN (`this token must not appear`) FALSE-POSITIVES on a comment
#           that explains why the token is wrong — `pcod`'s Patch::Apply guard,
#           fixed in #2871;
#         * a PRESENCE assertion (`this token must still appear`) FALSE-
#           NEGATIVES: the comment mentioning the token keeps the guard green
#           after the real call is deleted — `1j64`'s teardown-trap guard — and
#           a commented-out call satisfies it outright — `0vku`'s CI guard.
#       The presence direction is the dangerous one: it is silent.
#
#   (b) `#[cfg(test)]` TRUNCATION. `scripts/check-resize-reachability.sh`
#       stopped scanning at a file's first `#[cfg(test)]` marker. In
#       `server/src/server/state/mod.rs` that marker is a FIELD attribute on
#       line 342 of 4147, so the guard read the first 8% of the file and never
#       saw the composition site it exists to find. The convention it assumed
#       ("one test module at the bottom") is simply false for that file.
#
# So: one implementation, used by every Rust source-text guard, that strips
# comments without eating the code in front of them and that tracks test
# context STRUCTURALLY instead of by first-marker.
#
# USAGE
#
#   awk -f scripts/lib/rust-source-scan.awk \
#       -v strings=keep|blank -v scope=any|prod|test -v force_test=0|1 \
#       <file>...
#
#   RS_PATTERN   (env, REQUIRED)  ERE matched against the processed line.
#   RS_EXEMPT    (env, optional)  ERE matched against the RAW line; a matching
#                                 line is skipped before anything else. Used for
#                                 exemptions that key on text the processing
#                                 would erase (e.g. `env!("OUT_DIR")`).
#
#   Both patterns come from the environment rather than `-v` on purpose: `-v`
#   runs backslash-escape processing over its value, so `\(` and `\.` in an ERE
#   are mangled or undefined. `ENVIRON[]` is byte-exact.
#
#   strings=keep   (default) string literals are kept verbatim, so a pattern may
#                  match inside one. Required by matchers whose token IS a
#                  literal — `Command::new("git")`.
#   strings=blank  string BODIES are erased, delimiters kept (`"x"` -> `""`), so
#                  a literal's contents can never satisfy the pattern while a
#                  call assembled around one still can:
#                  `sqlx::query(&format!("..."))` keeps `sqlx::query(`.
#
#   scope=any      (default) report every match.
#   scope=prod     report only matches OUTSIDE test context. This is what a
#                  reachability/presence guard wants: a call site under
#                  `#[cfg(test)]` must not count as production reachability.
#   scope=test     report only matches INSIDE test context.
#
#   force_test=1   treat the whole file as test context regardless of structure.
#                  For files a PARENT declares as `#[cfg(test)] mod x;` — the
#                  attribute is not in this file, so nothing here can see it.
#
# Output, one line per hit:  <file>:<lineno>:<RAW line>
# The raw line is emitted so diagnostics show what the author actually wrote,
# comment and all.
#
# Exit status is awk's own. A missing RS_PATTERN exits 2.
#
# KNOWN LIMITS, both chosen to over-report rather than under-report
#
#   * Raw strings (`r#"..."#`) are paired by the same quote scan as ordinary
#     ones, so a raw body containing an unbalanced `"` mis-pairs. The result is
#     a line that reads as more code than it is, i.e. a possible false positive.
#   * Nothing here parses Rust. `scope=prod` can only ever UNDER-report
#     production hits if the tracker over-estimates test context; the tracker is
#     therefore written to disarm eagerly (see `rs_test_context`).

# ---------------------------------------------------------------------------
# Lexer
# ---------------------------------------------------------------------------

# Split one raw line into two processed forms, left in the globals
# `_rs_keep` (strings verbatim) and `_rs_blank` (string bodies erased).
# Comments are removed from both; `/* */` state carries across lines.
#
# The code IN FRONT of a trailing comment survives, which is the whole point:
# `foo(); // Patch::Apply` is still a call to `foo()`, and a guard that dropped
# the line would launder it.
function rs_split(line,   i, n, c, nxt, keep, blank) {
    n = length(line)
    i = 1
    keep = ""
    blank = ""
    while (i <= n) {
        c = substr(line, i, 1)
        nxt = substr(line, i + 1, 1)

        if (_rs_block) {
            if (c == "*" && nxt == "/") { _rs_block = 0; i += 2 } else { i += 1 }
            continue
        }

        # A char literal whose value IS a double quote must not open a phantom
        # string and swallow the rest of the line. Replaced by a placeholder of
        # the same shape so nothing downstream sees the quote.
        if (c == "'") {
            if (substr(line, i, 3) == "'\"'")   { keep = keep "'q'"; blank = blank "'q'"; i += 3; continue }
            if (substr(line, i, 4) == "'\\\"'") { keep = keep "'q'"; blank = blank "'q'"; i += 4; continue }
        }

        if (c == "/" && nxt == "/") break                       # line comment
        if (c == "/" && nxt == "*") { _rs_block = 1; i += 2; continue }

        if (c == "\"") {
            keep = keep "\""
            blank = blank "\"\""
            i += 1
            while (i <= n) {
                c = substr(line, i, 1)
                if (c == "\\") { keep = keep substr(line, i, 2); i += 2; continue }
                keep = keep c
                i += 1
                if (c == "\"") break
            }
            continue
        }

        keep = keep c
        blank = blank c
        i += 1
    }
    _rs_keep = keep
    _rs_blank = blank
}

function rs_braces(s,   i, n, c, d) {
    n = length(s); d = 0
    for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "{") d++
        else if (c == "}") d--
    }
    return d
}

function rs_parens(s,   i, n, c, d) {
    n = length(s); d = 0
    for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "(") d++
        else if (c == ")") d--
    }
    return d
}

# ---------------------------------------------------------------------------
# Test-context tracker
# ---------------------------------------------------------------------------

# Returns 1 if the current line is inside test code. Call it for EVERY line —
# it is a state machine, and skipping a line loses the state.
#
# `s` must be the string-blanked code form, so a brace inside a literal or a
# comment cannot unbalance the depth count.
#
# The rule is structural, never positional. A test attribute ARMS the tracker;
# the item it decorates then either opens a block (tracked to its matching close
# brace) or is a single item that ends on its own line. That second case is what
# the first-marker guards got wrong: `#[cfg(test)]` on a struct FIELD or a `mod
# x;` declaration introduces no block at all, and treating it as one truncated
# `state/mod.rs` at line 342 of 4147.
#
# Disarming is eager. An over-eager disarm reports a test hit as production,
# which is visible; an under-eager one silently swallows production code, which
# is the failure this file exists to prevent.
function rs_test_context(s,   d) {
    if (force_test == 1) return 1

    if (_rs_depth > 0) {
        _rs_depth += rs_braces(s)
        if (_rs_depth < 0) _rs_depth = 0
        return 1
    }

    if (s ~ /^[ \t]*#\[[ \t]*(cfg[ \t]*\([ \t]*test[ \t]*\)|test|tokio::test|rstest|async_std::test|proptest)/) {
        _rs_armed = 1
        _rs_parens = 0
        return 1
    }

    if (_rs_armed) {
        # Stacked attributes and blank / comment-only lines sit between the
        # attribute and the item it decorates.
        if (s ~ /^[ \t]*#\[/) return 1
        if (s ~ /^[ \t]*$/) return 1

        _rs_parens += rs_parens(s)
        d = rs_braces(s)
        if (d > 0) { _rs_depth = d; _rs_armed = 0; _rs_parens = 0; return 1 }

        # Still inside a multi-line signature -> the item has not started yet.
        if (_rs_parens > 0) return 1

        # A `;`- or `,`-terminated item: a field, a `mod x;`, a `use`, a const.
        # It ends here and introduces no block.
        _rs_armed = 0
        _rs_parens = 0
        return 1
    }

    return 0
}

# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

BEGIN {
    RS_PATTERN = ENVIRON["RS_PATTERN"]
    RS_EXEMPT = ENVIRON["RS_EXEMPT"]
    if (RS_PATTERN == "") {
        print "rust-source-scan.awk: RS_PATTERN must be set in the environment" > "/dev/stderr"
        exit 2
    }
    if (strings == "") strings = "keep"
    if (scope == "") scope = "any"
    if (strings != "keep" && strings != "blank") {
        print "rust-source-scan.awk: strings must be keep or blank, got: " strings > "/dev/stderr"
        exit 2
    }
    if (scope != "any" && scope != "prod" && scope != "test") {
        print "rust-source-scan.awk: scope must be any, prod or test, got: " scope > "/dev/stderr"
        exit 2
    }
}

FNR == 1 { _rs_block = 0; _rs_depth = 0; _rs_armed = 0; _rs_parens = 0 }

{
    rs_split($0)
    # Unconditional: the tracker is stateful and must see every line.
    in_test = rs_test_context(_rs_blank)

    if (scope == "prod" && in_test) next
    if (scope == "test" && !in_test) next
    if (RS_EXEMPT != "" && $0 ~ RS_EXEMPT) next

    if ((strings == "blank" ? _rs_blank : _rs_keep) ~ RS_PATTERN) {
        printf "%s:%d:%s\n", FILENAME, FNR, $0
    }
}
