import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  fetchUserConnectedModels,
  saveUserModelSelection,
  type UserModel,
  type UserModelSelection,
} from "@/api/userConfig";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";

import { OnboardingModelSetup } from "./OnboardingModelSetup";

vi.mock("@/api/userConfig", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/userConfig")>();
  return {
    ...actual,
    fetchUserConnectedModels: vi.fn(),
    saveUserModelSelection: vi.fn(),
  };
});

function model(id: string, name: string, recommended = false): UserModel {
  return {
    id,
    name,
    provider_id: id.split("/")[0] ?? "unknown",
    tool_call: true,
    recommended,
  } as UserModel;
}

function selection(
  lanes: UserModelSelection["lanes"] = {
    plan: [],
    implement: [],
    review: [],
  },
): UserModelSelection {
  return {
    lanes,
    maxSessions: {},
    diverseReview: true,
    diverseRefinement: true,
    laneLocked: false,
  };
}

describe("OnboardingModelSetup", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("automatically uses the only connected model for all three roles", async () => {
    const onlyModel = model("openai/gpt-5.3-codex", "GPT-5.3 Codex", true);
    vi.mocked(fetchUserConnectedModels).mockResolvedValue([onlyModel]);
    vi.mocked(saveUserModelSelection).mockResolvedValue(
      selection({
        plan: [onlyModel.id],
        implement: [onlyModel.id],
        review: [onlyModel.id],
      }),
    );
    const onSaved = vi.fn();
    const user = userEvent.setup();

    render(
      <OnboardingModelSetup
        targetId="__self__"
        selection={selection()}
        onSaved={onSaved}
      />,
    );

    expect(
      await screen.findByText(/selected it for all three roles/i),
    ).toBeInTheDocument();
    expect(getModelTrigger("Plan")).toHaveAccessibleName(
      /Plan model: GPT-5\.3 Codex · OpenAI/i,
    );
    expect(getModelTrigger("Code")).toHaveTextContent(onlyModel.name);
    expect(getModelTrigger("Review")).toHaveTextContent(onlyModel.name);

    await user.click(
      screen.getByRole("button", { name: "Save models and continue" }),
    );

    await waitFor(() =>
      expect(saveUserModelSelection).toHaveBeenCalledWith(
        "__self__",
        {
          plan: [onlyModel.id],
          implement: [onlyModel.id],
          review: [onlyModel.id],
        },
        { [onlyModel.id]: 3 },
        {
          laneMaxSessions: { plan: 1, implement: 1, review: 1 },
        },
      ),
    );
    expect(onSaved).toHaveBeenCalledOnce();
  });

  it("requires Plan, Code, and Review while allowing the same model in every role", async () => {
    const models = [
      model("openai/gpt-5.5", "GPT-5.5", true),
      model("openai/gpt-5.3-codex", "GPT-5.3 Codex"),
    ];
    vi.mocked(fetchUserConnectedModels).mockResolvedValue(models);
    vi.mocked(saveUserModelSelection).mockResolvedValue(
      selection({
        plan: [models[0]!.id],
        implement: [models[0]!.id],
        review: [models[0]!.id],
      }),
    );
    const user = userEvent.setup();

    render(
      <OnboardingModelSetup
        targetId="__self__"
        selection={selection()}
        onSaved={vi.fn()}
      />,
    );

    const save = await screen.findByRole("button", {
      name: "Save models and continue",
    });
    expect(save).toBeDisabled();

    for (const role of ["Plan", "Code", "Review"]) {
      await chooseModel(user, role, "gpt-5.5", models[0]!.name);
    }
    expect(save).toBeEnabled();
    await user.click(save);

    await waitFor(() =>
      expect(saveUserModelSelection).toHaveBeenCalledWith(
        "__self__",
        {
          plan: [models[0]!.id],
          implement: [models[0]!.id],
          review: [models[0]!.id],
        },
        { [models[0]!.id]: 3 },
        {
          laneMaxSessions: { plan: 1, implement: 1, review: 1 },
        },
      ),
    );
  });

  it("preserves connected fallbacks and the full caps map while dropping stale lane ids", async () => {
    const models = [
      model("openai/gpt-5.5", "GPT-5.5", true),
      model("openai/gpt-5.3-codex", "GPT-5.3 Codex"),
    ];
    const existing = selection({
      plan: [models[0]!.id, "retired/old-model", models[1]!.id],
      implement: [models[1]!.id],
      review: [models[0]!.id],
    });
    existing.maxSessions = {
      [models[0]!.id]: 4,
      "retired/old-model": 7,
      "unrelated/stored-cap": 9,
    };
    vi.mocked(fetchUserConnectedModels).mockResolvedValue(models);
    vi.mocked(saveUserModelSelection).mockResolvedValue(existing);
    const user = userEvent.setup();

    render(
      <OnboardingModelSetup
        targetId="__self__"
        selection={existing}
        onSaved={vi.fn()}
      />,
    );

    expect(await findModelTrigger("Plan")).toHaveTextContent(
      models[0]!.name,
    );
    expect(getModelTrigger("Code")).toHaveTextContent(
      models[1]!.name,
    );
    expect(screen.queryByText(/fallback/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/^Sessions$/i)).not.toBeInTheDocument();

    await chooseModel(user, "Plan", "gpt-5.3", models[1]!.name);
    await user.click(
      screen.getByRole("button", { name: "Save models and continue" }),
    );

    await waitFor(() =>
      expect(saveUserModelSelection).toHaveBeenCalledWith(
        "__self__",
        {
          plan: [models[1]!.id, models[0]!.id],
          implement: [models[1]!.id],
          review: [models[0]!.id],
        },
        {
          [models[0]!.id]: 4,
          [models[1]!.id]: 2,
          "retired/old-model": 7,
          "unrelated/stored-cap": 9,
        },
        {
          laneMaxSessions: { plan: 1, implement: 1, review: 1 },
        },
      ),
    );
  });

  it("searches the full catalog in the same height-capped picker used by Model Roles", async () => {
    const recommended = model("acme/zeta", "Zeta", true);
    const hidden = model(
      "acme/accounts/acme/models/legacy.v2.5",
      "Legacy v2.5",
    );
    const existing = selection({
      plan: [],
      implement: [recommended.id],
      review: [recommended.id],
    });
    vi.mocked(fetchUserConnectedModels).mockResolvedValue([
      recommended,
      hidden,
    ]);
    vi.mocked(saveUserModelSelection).mockResolvedValue(
      selection({
        plan: [hidden.id],
        implement: [recommended.id],
        review: [recommended.id],
      }),
    );
    const user = userEvent.setup();

    render(
      <OnboardingModelSetup
        targetId="__self__"
        selection={existing}
        onSaved={vi.fn()}
      />,
    );

    await user.click(await findModelTrigger("Plan"));
    const dialog = await screen.findByRole("dialog", {
      name: "Select plan model",
    });
    expect(dialog).toHaveClass("sm:max-w-xl");
    const list = dialog.querySelector('[data-slot="command-list"]');
    expect(list).toHaveClass("max-h-[300px]", "overflow-y-auto");
    expect(within(dialog).queryByText(hidden.name)).not.toBeInTheDocument();

    const searchInput = within(dialog).getByPlaceholderText("Search models…");
    await user.type(searchInput, "sss");
    expect(searchInput).toHaveValue("sss");
    await user.clear(searchInput);

    await user.type(
      searchInput,
      "   legacy.v2.5   ",
    );
    const hiddenResult = await within(dialog).findByText(hidden.name);
    await user.click(hiddenResult.closest('[data-slot="command-item"]')!);

    expect(getModelTrigger("Plan")).toHaveTextContent(
      `${hidden.name} · Acme`,
    );
    expect(getModelTrigger("Plan")).toHaveAccessibleName(
      `Plan model: ${hidden.name} · Acme`,
    );

    await user.click(getModelTrigger("Plan"));
    const reopenedDialog = await screen.findByRole("dialog", {
      name: "Select plan model",
    });
    await user.type(
      within(reopenedDialog).getByPlaceholderText("Search models…"),
      "legacy.v2.5",
    );
    const selectedResult = await within(reopenedDialog).findByText(hidden.name);
    expect(selectedResult.closest('[data-slot="command-item"]')).toHaveAttribute(
      "aria-current",
      "true",
    );
    await user.click(
      within(reopenedDialog).getByRole("button", { name: "Close" }),
    );

    await user.click(
      screen.getByRole("button", { name: "Save models and continue" }),
    );

    await waitFor(() =>
      expect(saveUserModelSelection).toHaveBeenCalledWith(
        "__self__",
        {
          plan: [hidden.id],
          implement: [recommended.id],
          review: [recommended.id],
        },
        {
          [hidden.id]: 1,
          [recommended.id]: 2,
        },
        {
          laneMaxSessions: { plan: 1, implement: 1, review: 1 },
        },
      ),
    );
  });

  it("sets 1–3 parallel agents per lane and raises a shared model cap to the aggregate", async () => {
    const onlyModel = model("openai/gpt-5.3-codex", "GPT-5.3 Codex", true);
    vi.mocked(fetchUserConnectedModels).mockResolvedValue([onlyModel]);
    vi.mocked(saveUserModelSelection).mockResolvedValue(
      selection({
        plan: [onlyModel.id],
        implement: [onlyModel.id],
        review: [onlyModel.id],
      }),
    );
    const user = userEvent.setup();

    render(
      <OnboardingModelSetup
        targetId="__self__"
        selection={selection()}
        onSaved={vi.fn()}
      />,
    );

    expect(
      await screen.findByText(/maximum number djinn can run/i),
    ).toHaveTextContent(/1–3/i);
    for (const role of ["Plan", "Code", "Review"]) {
      expect(
        screen.getByRole("combobox", { name: `${role} parallel agents` }),
      ).toHaveTextContent("1");
    }

    await chooseParallelAgents(user, "Plan", 2);
    await chooseParallelAgents(user, "Code", 3);
    await user.click(
      screen.getByRole("button", { name: "Save models and continue" }),
    );

    await waitFor(() =>
      expect(saveUserModelSelection).toHaveBeenCalledWith(
        "__self__",
        {
          plan: [onlyModel.id],
          implement: [onlyModel.id],
          review: [onlyModel.id],
        },
        { [onlyModel.id]: 6 },
        {
          laneMaxSessions: { plan: 2, implement: 3, review: 1 },
        },
      ),
    );
  });
});

async function chooseModel(
  user: ReturnType<typeof userEvent.setup>,
  role: string,
  query: string,
  name: string,
) {
  await user.click(getModelTrigger(role));
  const dialog = await screen.findByRole("dialog", {
    name: `Select ${role.toLowerCase()} model`,
  });
  await user.type(
    within(dialog).getByPlaceholderText("Search models…"),
    query,
  );
  const result = await within(dialog).findByText(name);
  await user.click(result.closest('[data-slot="command-item"]')!);
}

function getModelTrigger(role: string): HTMLElement {
  return screen.getByRole("button", {
    name: new RegExp(`^${role} model:`, "i"),
  });
}

function findModelTrigger(role: string): Promise<HTMLElement> {
  return screen.findByRole("button", {
    name: new RegExp(`^${role} model:`, "i"),
  });
}

async function chooseParallelAgents(
  user: ReturnType<typeof userEvent.setup>,
  role: string,
  value: number,
) {
  await user.click(
    screen.getByRole("combobox", { name: `${role} parallel agents` }),
  );
  await user.click(await screen.findByRole("option", { name: String(value) }));
}
