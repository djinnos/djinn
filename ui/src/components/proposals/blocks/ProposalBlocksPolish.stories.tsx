import type { Meta, StoryObj } from "@storybook/react-vite";

import { ChecklistBlock } from "./ChecklistBlock";
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
