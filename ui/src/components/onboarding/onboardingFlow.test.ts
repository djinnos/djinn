import { describe, expect, it } from "vitest";

import type { Project } from "@/api/server";

import { resolveOnboardingDestination } from "./onboardingFlow";

const project = {
  id: "project-1",
  name: "Example",
  github_owner: "djinnos",
  github_repo: "example",
} as Project;

const ready = {
  hasProject: true,
  hasProvider: true,
  hasModels: true,
  projectNeedingImage: null,
  projectError: null,
  serverStatus: "connected" as const,
};

describe("resolveOnboardingDestination", () => {
  it("waits for readiness facts instead of flashing the app", () => {
    expect(
      resolveOnboardingDestination({ ...ready, hasProject: null }),
    ).toBe("checking");
    expect(
      resolveOnboardingDestination({ ...ready, hasProvider: null }),
    ).toBe("checking");
  });

  it("does not fail open during an initial or mid-onboarding server outage", () => {
    expect(
      resolveOnboardingDestination({
        ...ready,
        serverStatus: "error",
        hasProject: null,
        hasProvider: null,
        hasModels: null,
      }),
    ).toBe("connection-error");
    expect(
      resolveOnboardingDestination({
        ...ready,
        serverStatus: "error",
        hasModels: false,
      }),
    ).toBe("connection-error");
  });

  it("keeps the application shell during an outage only after every server gate resolved complete", () => {
    expect(
      resolveOnboardingDestination({ ...ready, serverStatus: "error" }),
    ).toBe("app");
    expect(
      resolveOnboardingDestination({
        ...ready,
        serverStatus: "error",
        projectError: "status unresolved",
      }),
    ).toBe("connection-error");
  });

  it("surfaces a retry state when project readiness fails", () => {
    expect(
      resolveOnboardingDestination({
        ...ready,
        hasProject: null,
        projectError: "offline",
      }),
    ).toBe("project-error");
  });

  it("starts the repository before models so stack detection overlaps setup", () => {
    expect(
      resolveOnboardingDestination({
        ...ready,
        hasProject: false,
        hasProvider: false,
        hasModels: false,
      }),
    ).toBe("repository");
  });

  it("requires provider and every effective model lane before image setup", () => {
    expect(
      resolveOnboardingDestination({ ...ready, hasProvider: false }),
    ).toBe("models");
    expect(resolveOnboardingDestination({ ...ready, hasModels: false })).toBe(
      "models",
    );
    expect(
      resolveOnboardingDestination({ ...ready, projectNeedingImage: project }),
    ).toBe("image");
  });

  it("opens the app only when every required setup fact is complete", () => {
    expect(resolveOnboardingDestination(ready)).toBe("app");
  });
});
