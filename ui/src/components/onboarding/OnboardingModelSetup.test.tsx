import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  fetchUserConnectedModels,
  saveUserModelSelection,
  type UserModel,
  type UserModelSelection,
} from "@/api/userConfig";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

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
    expect(screen.getByLabelText("Plan model")).toHaveValue(onlyModel.id);
    expect(screen.getByLabelText("Code model")).toHaveValue(onlyModel.id);
    expect(screen.getByLabelText("Review model")).toHaveValue(onlyModel.id);

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
        { [onlyModel.id]: 1 },
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
      await user.selectOptions(screen.getByLabelText(`${role} model`), models[0]!.id);
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
        { [models[0]!.id]: 1 },
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

    expect(await screen.findByLabelText("Plan model")).toHaveValue(models[0]!.id);
    expect(screen.getByLabelText("Code model")).toHaveValue(models[1]!.id);
    expect(screen.queryByText(/fallback/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/sessions/i)).not.toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Plan model"), models[1]!.id);
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
          [models[1]!.id]: 1,
          "retired/old-model": 7,
          "unrelated/stored-cap": 9,
        },
      ),
    );
  });
});
