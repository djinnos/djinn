import { afterEach, describe, expect, it } from "vitest";

import {
  assignmentSummary,
  clearFirstRunDismissal,
  dismissFirstRun,
  firstRunDismissKey,
  isFirstRunDismissed,
} from "./firstRun";

const USER = "user-123";

afterEach(() => {
  window.localStorage.clear();
});

describe("firstRun dismissal", () => {
  it("keys the flag by user id", () => {
    expect(firstRunDismissKey(USER)).toBe(
      "djinn.onboarding.firstRun.dismissed.user-123",
    );
  });

  it("is not dismissed by default", () => {
    expect(isFirstRunDismissed(USER)).toBe(false);
  });

  it("records and reads a dismissal", () => {
    dismissFirstRun(USER);
    expect(isFirstRunDismissed(USER)).toBe(true);
  });

  it("scopes dismissal per user", () => {
    dismissFirstRun(USER);
    expect(isFirstRunDismissed("other-user")).toBe(false);
  });

  it("treats a null/empty user as never-dismissed and is a no-op to write", () => {
    expect(isFirstRunDismissed(null)).toBe(false);
    dismissFirstRun(null);
    expect(isFirstRunDismissed(null)).toBe(false);
  });

  it("clears a dismissal", () => {
    dismissFirstRun(USER);
    clearFirstRunDismissal(USER);
    expect(isFirstRunDismissed(USER)).toBe(false);
  });
});

describe("assignmentSummary", () => {
  it("uses the primary model id per lane, provider-stripped", () => {
    expect(
      assignmentSummary({
        plan: ["anthropic/opus", "openai/gpt-5"],
        implement: ["openai/gpt-5-mini"],
        review: ["fireworks/kimi"],
      }),
    ).toBe("Plan w/ opus · Build w/ gpt-5-mini · Review w/ kimi");
  });

  it("falls back to 'default' for empty lanes", () => {
    expect(
      assignmentSummary({ plan: [], implement: [], review: [] }),
    ).toBe("Plan w/ default · Build w/ default · Review w/ default");
  });

  it("handles ids without a provider prefix", () => {
    expect(
      assignmentSummary({ plan: ["bare-model"], implement: [], review: [] }),
    ).toBe("Plan w/ bare-model · Build w/ default · Review w/ default");
  });
});
