import type { Meta, StoryObj } from "@storybook/react-vite";

import { ChecklistBlock } from "./ChecklistBlock";
import { DiffBlock } from "./DiffBlock";
import { TabsBlock } from "./TabsBlock";

/**
 * Stories used to visually reproduce + verify two block polish fixes
 * (checklist unchecked-marker glyph; tabs-header vertical scrollbar). These
 * render in a real browser via Playwright/Storybook where the bugs are visible
 * (jsdom cannot surface them).
 */
const meta = {
  title: "Proposals/Blocks/Polish",
  parameters: { layout: "padded" },
} satisfies Meta;

export default meta;

const CHECKLIST_BODY = [
  "- [x] Schema migration written",
  "- [ ] Backfill job verified",
  "- [ ] Docs updated — link the runbook",
  "- [x] Rollback plan reviewed",
].join("\n");

export const Checklist: StoryObj = {
  render: () => (
    <div style={{ maxWidth: 520 }}>
      <ChecklistBlock id="checklist-demo" attributes={{}}>
        {CHECKLIST_BODY}
      </ChecklistBlock>
    </div>
  ),
};

const TABS_ATTR = JSON.stringify([
  { label: "Overview", body: "Plain markdown body for the overview tab." },
  { label: "Details", body: "Details body with a list:\n\n- one\n- two" },
  { label: "Risks", body: "Risk notes for the third tab." },
]);

export const Tabs: StoryObj = {
  render: () => (
    <div style={{ maxWidth: 520 }}>
      <TabsBlock id="tabs-demo" attributes={{ tabs: TABS_ATTR }}>
        {null}
      </TabsBlock>
    </div>
  ),
};

// A `lang="rust"` diff (modelled on proposal 3in8's run_chat_loop change) used
// to verify the diff block now Prism-highlights the code UNDER the +/- tint.
const RUST_DIFF = [
  "@@ -1,9 +1,11 @@",
  " async fn run_chat_loop(state: &State, session_id: Uuid) -> Result<()> {",
  "     let mut turns = 0u32;",
  "-    let model = state.default_model.clone();",
  "+    let model = resolve_model(state, session_id).await?;",
  "+    let breaker = state.breaker_for(&model);",
  "     loop {",
  "-        let reply = call_model(&model, &history).await?;",
  "+        let reply = call_model(&model, &history).await.context(\"chat turn\")?;",
  "         history.push(reply);",
  "         turns += 1;",
  "     }",
  " }",
].join("\n");

export const RustDiff: StoryObj = {
  render: () => (
    <div style={{ maxWidth: 760 }}>
      <DiffBlock
        id="diff-rust-demo"
        attributes={{ filename: "server/src/chat/loop.rs", lang: "rust" }}
      >
        {RUST_DIFF}
      </DiffBlock>
    </div>
  ),
};
