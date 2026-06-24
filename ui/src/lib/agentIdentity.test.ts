import { describe, expect, it } from "vitest";
import { getAgentIdentity, getAgentAvatar } from "./agentIdentity";

describe("getAgentIdentity", () => {
  it("returns Worker identity for 'worker'", () => {
    const id = getAgentIdentity("worker");
    expect(id.label).toBe("Worker");
    expect(id.color).toBe("text-blue-400");
  });

  it("returns Advocate identity for 'advocate'", () => {
    const id = getAgentIdentity("advocate");
    expect(id.label).toBe("Advocate");
    expect(id.color).toBe("text-sky-400");
  });

  it("returns Adversary identity for 'adversary'", () => {
    const id = getAgentIdentity("adversary");
    expect(id.label).toBe("Adversary");
    expect(id.color).toBe("text-orange-400");
  });

  it("returns Judge identity for 'judge'", () => {
    const id = getAgentIdentity("judge");
    expect(id.label).toBe("Judge");
    expect(id.color).toBe("text-violet-400");
  });

  it("returns Lead identity for 'pm'", () => {
    const id = getAgentIdentity("pm");
    expect(id.label).toBe("Lead");
    expect(id.color).toBe("text-red-400");
  });

  it("returns Epic Reviewer identity for 'epic_reviewer'", () => {
    const id = getAgentIdentity("epic_reviewer");
    expect(id.label).toBe("Epic Reviewer");
    expect(id.color).toBe("text-teal-400");
  });

  it("returns fallback for unknown agent type", () => {
    const id = getAgentIdentity("unknown_role");
    expect(id.label).toBe("Agent");
    expect(id.color).toBe("text-muted-foreground");
  });

  it("returns fallback for undefined agent type", () => {
    const id = getAgentIdentity(undefined);
    expect(id.label).toBe("Worker");
  });

  it("returns distinct avatars for tribunal roles", () => {
    const advocate = getAgentAvatar("advocate");
    const adversary = getAgentAvatar("adversary");
    const judge = getAgentAvatar("judge");
    const worker = getAgentAvatar("worker");

    expect(advocate).toBeTruthy();
    expect(adversary).toBeTruthy();
    expect(judge).toBeTruthy();

    // Tribunal avatars should be different from each other and from worker.
    expect(advocate).not.toBe(adversary);
    expect(advocate).not.toBe(judge);
    expect(adversary).not.toBe(judge);
    expect(advocate).not.toBe(worker);
  });
});
