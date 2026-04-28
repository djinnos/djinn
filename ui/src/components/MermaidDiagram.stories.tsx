import type { Meta, StoryObj } from "@storybook/react-vite";

import { MermaidDiagram } from "./MermaidDiagram";

/**
 * PR D4 stories. Exercises the reusable Mermaid renderer across:
 *   - a `flowchart TD` (the ImpactFlowModal default),
 *   - a `sequenceDiagram` (smoke-test for the chat-citation use case in D5),
 *   - a deliberately bad source so we can eyeball the error fallback.
 */

const meta = {
  title: "Codegraph/MermaidDiagram",
  component: MermaidDiagram,
  parameters: {
    layout: "padded",
  },
  decorators: [
    (Story) => (
      <div className="mx-auto max-w-2xl rounded-md border border-border/40 bg-background/50 p-4">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof MermaidDiagram>;

export default meta;

type Story = StoryObj<typeof meta>;

export const Flowchart: Story = {
  args: {
    source: [
      "flowchart TD",
      "  a[Architect] --> b[Reviewer]",
      "  b --> c[Worker]",
      "  c --> d[Quality Gate]",
    ].join("\n"),
  },
};

export const Sequence: Story = {
  args: {
    source: [
      "sequenceDiagram",
      "  participant U as User",
      "  participant A as Architect",
      "  participant W as Worker",
      "  U->>A: /chat \"add feature X\"",
      "  A->>W: dispatch task",
      "  W-->>A: PR opened",
      "  A-->>U: summary + citations",
    ].join("\n"),
  },
};

export const ImpactFlowchart: Story = {
  args: {
    source: [
      "flowchart TD",
      '  target["do_thing"]:::target',
      '  subgraph depth_1["Direct (depth 1)"]',
      '    n0["caller_a"]',
      '    n1["caller_b"]',
      "  end",
      '  subgraph depth_2["Depth 2"]',
      '    n2["deep_caller"]',
      "  end",
      "  n0 --> target",
      "  n1 --> target",
      "  n2 --> n0",
      "  classDef target fill:#fde68a,stroke:#b45309;",
    ].join("\n"),
  },
};

export const BadSource: Story = {
  args: {
    source: "this is not valid mermaid syntax !!!",
  },
};
