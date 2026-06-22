//! The single `define_blocks!` invocation holding all v1 proposal blocks.
//!
//! This is the byte-heavy part — the long authoring descriptions — but it is now
//! one list, and adding a block is a single declaration here. The expanded
//! registry data must remain byte-identical to the previous hand-written
//! `PROPOSAL_BLOCK_REGISTRY`; this is a refactor, not a content change.

use std::collections::{BTreeMap, HashSet};

use super::macros::{array_field, define_blocks, fields, object_field, string_field};
use super::types::ProposalBlockDefinition;

// `BlockType`, `CANONICAL_BLOCK_TYPES`, and `PROPOSAL_BLOCK_REGISTRY` are all
// produced by this single invocation. Irregular nested field schemas use the
// `nested(<expr>)` escape. QuestionForm is kept LAST.
define_blocks! {
    RichText => "rich-text", "RichText",
        fields { "content" => string },

    Diagram => "diagram", "Diagram",
        fields {
            "type" => (enum ["mermaid", "plantuml", "svg"]),
            "source" => string,
        },

    AnnotatedCode => "annotated-code", "AnnotatedCode",
        fields {
            "language" => string,
            "code" => string,
            "annotations" => (nested(array_field(object_field(fields(vec![
                ("line", string_field()),
                ("note", string_field()),
            ]))))),
        },

    ApiEndpoint => "api-endpoint", "ApiEndpoint",
        fields {
            "method" => string,
            "path" => string,
            "description" => string,
            "request_schema" => string,
            "response_schema" => string,
        },

    Decisions => "decisions", "Decisions",
        desc = "ADR-style architecture decision records. The block CHILDREN are \
                markdown: write ONE decision per `### Title` heading (`##` also \
                accepted). DECLARE each decision's status with an explicit \
                `Status:` line directly under the heading — one of exactly \
                `proposed`, `accepted`, `rejected`, `deprecated`, `superseded` \
                (or an inline `[accepted]` marker on the heading line). Status \
                comes ONLY from this declared token; it is NEVER inferred from \
                words in the prose. Omit the line for no status badge. A \
                superseded record may name its replacement: \
                `Status: superseded by #3`. Optionally structure the body with \
                `Context`, `Decision`, and `Consequences` sub-section labels \
                (plain label, `Context:`, `**Decision**`, or `#### Consequences` \
                all work); otherwise the body renders as freeform markdown. \
                Example: `<Decisions id=\"x\">\\n### Use JWT for stateless auth\\n\
                Status: accepted\\n\\nContext\\nWe scale horizontally.\\n\\n\
                Decision\\nAdopt short-lived JWTs.\\n</Decisions>`. A body with no \
                `##`/`###` headings falls back to a plain markdown render.",
        fields {
            "items" => (nested(array_field(object_field(fields(vec![
                ("decision", string_field()),
                ("rationale", string_field()),
                ("status", string_field()),
            ]))))),
        },

    FileTree => "file-tree", "FileTree",
        desc = "A file & change tree. The block CHILDREN are the tree text: \
                either an indented ASCII tree (folders end in `/`, files sit \
                under a deeper indent) OR one slash-path per line. DECLARE each \
                file's change status with a single leading token immediately \
                followed by whitespace, then the path: `+` = added/new, `~` = \
                modified, `-` = removed/deleted, `>` = renamed/moved; NO token \
                means unchanged. Status comes ONLY from this token — words like \
                `new`/`modified` are NOT inferred. A trailing note after the \
                path (e.g. `~ src/x.rs — wire the route` or `src/y.ts: helper`) \
                renders as a muted caption. Example: \
                `<FileTree id=\"x\" root=\"src\">\\n+ src/added.rs\\n~ src/main.rs \
                bump version\\n- src/old.rs\\n</FileTree>`. Unparseable bodies \
                fall back to a raw monospace render.",
        fields {
            "root" => string,
            "entries" => (nested(array_field(object_field(fields(vec![
                ("path", string_field()),
                ("kind", super::macros::enum_string_field(vec!["file", "dir"])),
            ]))))),
        },

    Diff => "diff", "Diff",
        desc = "A before/after code diff. The block CHILDREN are a git-style \
                unified diff: optional `diff --git`/`---`/`+++`/`@@ -old,+new @@` \
                headers, then body lines each prefixed with `+` (added line), \
                `-` (removed line), or a single leading space (unchanged context \
                line). Old and new line numbers are reconstructed from the `@@` \
                hunk headers. Set the optional `filename` attribute to the file \
                path (shown in the header) and the optional `lang` attribute to a \
                language hint (e.g. `ts`, `rust`). Example: \
                `<Diff id=\"x\" filename=\"src/add.ts\" lang=\"ts\">\\n@@ -1,2 +1,2 @@\\n \
                const a = 1;\\n-const b = 2;\\n+const b = 3;\\n</Diff>`. If the \
                children are not a valid unified diff the renderer falls back to a \
                plain code block, so always emit real `+`/`-`/` ` prefixed lines.",
        fields {
            "filename" => string,
            "lang" => string,
        },

    Callout => "callout", "Callout",
        desc = "An emphasized admonition card with a `tone` and a markdown \
                body. Set the optional `tone` attribute to one of `info` \
                (default), `decision`, `risk`, `warning`, or `success`; it \
                drives the card's color and icon. The block CHILDREN are the \
                markdown body. Use it to flag a risk, a decision, or an \
                important note so it stands out from surrounding prose. \
                Example: `<Callout id=\"x\" tone=\"warning\">\\nThis runs on \
                the hot path.\\n</Callout>`.",
        fields {
            "tone" => (enum ["info", "decision", "risk", "warning", "success"]),
        },

    Checklist => "checklist", "Checklist",
        desc = "A read-only checklist of items with their authored \
                done-state. The block CHILDREN are GitHub task-list lines: \
                `- [x] done item` (checked) or `- [ ] todo item` \
                (unchecked); plain `- item` bullets render unchecked. Add an \
                optional trailing note after ` — ` (em-dash) or two spaces. \
                The boxes are NOT toggleable by viewers — they reflect the \
                authored state only. Use it for acceptance criteria or \
                done-criteria. Example: `<Checklist id=\"x\">\\n- [x] Schema \
                written\\n- [ ] Docs updated — link the runbook\\n</Checklist>`.",
        fields {},

    JsonExplorer => "json-explorer", "JsonExplorer",
        desc = "An interactive, collapsible typed JSON tree (browser-devtools \
                / Postman style). The block CHILDREN are a single JSON \
                document — an object, array, or primitive. The renderer \
                `JSON.parse`s the children and walks the value: object/array \
                nodes show a chevron + a one-line summary (`3 keys` / `5 \
                items`) and expand/collapse, and leaf values are \
                type-colored (string, number, boolean, null). It is \
                read-only. If the children are not valid JSON the renderer \
                falls back to a plain code block, so always emit a single \
                valid JSON document. Set the optional `title` attribute to a \
                heading shown in the block header. Example: \
                `<JsonExplorer id=\"x\" title=\"Sample response\">\\n{\\n  \
                \"id\": \"abc\",\\n  \"active\": true\\n}\\n</JsonExplorer>`.",
        fields {},

    Tabs => "tabs", "Tabs",
        desc = "A tabbed container: one tab strip whose active panel \
                recursively renders its own proposal-MDX body. The block is \
                SELF-CLOSING; all content lives in the `tabs` attribute as a \
                JSON array passed via a `{...}` expression: \
                `tabs={[{ \"label\": \"...\", \"body\": \"...mdx...\" }, ...]}`. \
                Each entry has a `label` (the tab button text) and a `body` \
                (a normal proposal-MDX string — the SAME grammar as the \
                top-level proposal body, so it may itself contain blocks like \
                `<Callout .../>` and inline **markdown**). The body is parsed \
                and rendered recursively (nesting is depth-capped). The first \
                tab is shown by default. Author the JSON with proper escaping \
                (newlines as `\\n`, quotes as `\\\"`). If the attribute is \
                missing or not valid JSON the block falls back gracefully. \
                Example: `<Tabs id=\"x\" tabs={[{ \"label\": \"Overview\", \
                \"body\": \"<Callout id=\\\"c\\\">Hi</Callout>\" }, { \"label\": \
                \"API\", \"body\": \"some **markdown**\" }]} />`.",
        fields { "tabs" => string },

    Columns => "columns", "Columns",
        desc = "A responsive multi-column layout container: N columns side by \
                side on wide screens, stacked on narrow ones. The block is \
                SELF-CLOSING; all content lives in the `columns` attribute as \
                a JSON array passed via a `{...}` expression: \
                `columns={[{ \"body\": \"...mdx...\" }, { \"body\": \"...mdx...\" }]}`. \
                Each entry has a `body` (a normal proposal-MDX string — the \
                SAME grammar as the top-level proposal body, so it may itself \
                contain blocks like `<ApiEndpoint .../>` and inline **markdown**). \
                Each column body is parsed and rendered recursively (nesting is \
                depth-capped). Author the JSON with proper escaping (newlines \
                as `\\n`, quotes as `\\\"`). If the attribute is missing or not \
                valid JSON the block falls back gracefully. Example: \
                `<Columns id=\"x\" columns={[{ \"body\": \"### Before\\nold\" }, \
                { \"body\": \"### After\\nnew\" }]} />`.",
        fields { "columns" => string },

    Wireframe => "wireframe", "Wireframe",
        desc = "A low-fi HTML wireframe of ONE screen, rendered SAFELY in a \
                sandboxed iframe with the `--wf-*` design tokens. The block \
                CHILDREN are a self-contained HTML FRAGMENT of a single screen \
                — real product layout and real content, not lorem. Author PLAIN \
                semantic HTML and let the renderer style it: bare \
                `h1`/`h2`/`h3`/`p`/`button`/`input`/`a`/`hr` are auto-themed (no \
                classes needed) and helper classes carry the rest — `.wf-card` / \
                `.wf-box` (a bordered padded container), `.wf-pill` / `.wf-chip` \
                (a rounded tag; add `.accent` for the filled variant), \
                `.wf-muted` (secondary text), and `button.primary` (or \
                `[data-primary]`) for the accent button. For ANY custom color use \
                the `--wf-*` tokens, NEVER a hex literal: `--wf-ink` (text), \
                `--wf-muted`, `--wf-line` (borders), `--wf-paper` (page bg), \
                `--wf-card` (surface), `--wf-accent` / `--wf-accent-fg` / \
                `--wf-accent-soft`, `--wf-warn`, `--wf-ok`, `--wf-radius`. Lay out \
                with inline flex/grid `style` (`display:flex;gap:10px;\
                padding:16px;height:100%`). DO NOT emit `<html>`/`<body>`/\
                `<style>`/`<script>` tags or set `font-family` — the renderer owns \
                the document chrome and font. For icon-only controls or leading \
                icons, write an EMPTY marker `<span data-icon=\"mail\"></span>` \
                (or `<i data-icon=\"search\"></i>`); the renderer swaps it for an \
                inline SVG (names incl. mail/search/menu/x/check/plus/user/bell/\
                settings/chevronDown/etc., aliases supported). It is NOT a \
                scripting surface — `<script>`, `on*=` handlers, and \
                `javascript:` URIs are rejected at validation and stripped at \
                render. Set the `surface` attribute to match the real footprint: \
                `browser` (a web page w/ chrome), `desktop` (an app shell), \
                `mobile` (a phone screen), `popover` (a small dropdown/menu), or \
                `panel` (a side panel/inspector); default `desktop`. Example: \
                `<Wireframe id=\"x\" surface=\"browser\">\\n<div \
                style=\\\"display:flex;flex-direction:column;gap:10px;\
                padding:16px;height:100%\\\"><h1>Sign in</h1><div \
                class=\\\"wf-card\\\"><label>Email<input value=\\\"a@b.co\\\" />\
                </label><button class=\\\"primary\\\">Sign in</button></div></div>\
                \\n</Wireframe>`.",
        fields {
            "surface" => (enum ["browser", "desktop", "mobile", "popover", "panel"]),
        },

    QuestionForm => "question-form", "QuestionForm",
        fields {
            "title" => string,
            "questions" => (nested(array_field(object_field(fields(vec![
                ("question", string_field()),
                ("kind", super::macros::enum_string_field(vec!["text", "single", "multi"])),
                ("options", array_field(string_field())),
            ]))))),
        },
}

pub fn proposal_block_registry() -> BTreeMap<&'static str, ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY.clone()
}

pub fn proposal_block_definition_for_tag(tag: &str) -> Option<&'static ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .find(|definition| definition.tag == tag)
}

pub fn proposal_block_tags() -> HashSet<&'static str> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .map(|definition| definition.tag)
        .collect()
}
