import { afterEach, describe, expect, it } from "vitest";

import {
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
