import type { Epic, Project, Task, CiGateSnapshot } from "@/api/types";

export const mockProjectA: Project = {
  id: "project-1",
  name: "Project One",
  github_owner: "djinn",
  github_repo: "project-one",
  description: "First test project",
};

export const mockProjectB: Project = {
  id: "project-2",
  name: "Project Two",
  github_owner: "djinn",
  github_repo: "project-two",
  description: "Second test project",
};

export const mockProjects: Project[] = [mockProjectA, mockProjectB];

export const mockEpicA: Epic = {
  id: "epic-1",
  title: "Epic One",
  description: "First epic",
  status: "open",
  owner: null,
  priority: 1,
  issue_type: "epic",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

export const mockEpicB: Epic = {
  id: "epic-2",
  title: "Epic Two",
  description: "Second epic",
  status: "closed",
  owner: "alice",
  priority: 2,
  issue_type: "epic",
  created_at: "2026-01-02T00:00:00Z",
  updated_at: "2026-01-02T00:00:00Z",
};

export const mockTaskA: Task = {
  id: "task-1",
  title: "Task One",
  description: "First task",
  status: "open",
  owner: null,
  priority: 1,
  issue_type: "task",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  epic_id: mockEpicA.id,
  project_id: mockProjectA.id,
  labels: ["frontend"],
};

export const mockTaskB: Task = {
  id: "task-2",
  title: "Task Two",
  description: "Second task",
  status: "closed",
  owner: "bob",
  priority: 2,
  issue_type: "task",
  created_at: "2026-01-02T00:00:00Z",
  updated_at: "2026-01-02T00:00:00Z",
  epic_id: mockEpicB.id,
  project_id: mockProjectB.id,
  labels: ["backend"],
};

export const mockTaskC: Task = {
  id: "task-3",
  title: "Task Three",
  description: "Third task",
  status: "open",
  owner: "carol",
  priority: 3,
  issue_type: "task",
  created_at: "2026-01-03T00:00:00Z",
  updated_at: "2026-01-03T00:00:00Z",
  epic_id: mockEpicA.id,
  project_id: mockProjectA.id,
  labels: ["frontend", "urgent"],
};

// ── CI gate snapshot fixtures ────────────────────────────────────────────────

export const mockCiPassing: CiGateSnapshot = {
  status: "passing",
  head_sha: "abc1234567890def",
  blocking_required_check_names: [],
  first_seen_at: "2026-06-01T00:00:00Z",
  last_seen_at: "2026-06-01T00:05:00Z",
  same_signature_count: 1,
  pr_number: 42,
};

export const mockCiPending: CiGateSnapshot = {
  status: "pending",
  head_sha: "def4567890abcdef",
  blocking_required_check_names: [],
  first_seen_at: "2026-06-01T00:00:00Z",
  last_seen_at: "2026-06-01T00:01:00Z",
  same_signature_count: 1,
  pr_number: 43,
};

export const mockCiFailing: CiGateSnapshot = {
  status: "failing",
  head_sha: "failing1234567890",
  blocking_required_check_names: ["lint", "tests"],
  failure_fingerprint: "lint+tests@failing1234567890",
  first_seen_at: "2026-06-01T00:00:00Z",
  last_seen_at: "2026-06-01T00:10:00Z",
  same_signature_count: 3,
  last_remediation_base_sha: "base1234567890abc",
  pr_number: 44,
};

export const mockCiUnknown: CiGateSnapshot = {
  status: "unknown",
  head_sha: "unknown1234567890",
  blocking_required_check_names: [],
  first_seen_at: "2026-06-01T00:00:00Z",
  last_seen_at: "2026-06-01T00:00:00Z",
  same_signature_count: 0,
  pr_number: 45,
};
