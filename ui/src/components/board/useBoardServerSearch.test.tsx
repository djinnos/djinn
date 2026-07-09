import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import type { ReactNode } from "react";
import {
  SEARCH_DEBOUNCE_MS,
  useBoardServerSearch,
} from "./useBoardServerSearch";
import { searchTasksAcrossProjects } from "@/api/server";
import { projectStore } from "@/stores/projectStore";
import { boardSearchStore } from "@/stores/boardSearchStore";

vi.mock("@/api/server", () => ({
  searchTasksAcrossProjects: vi.fn().mockResolvedValue([]),
}));

const searchMock = vi.mocked(searchTasksAcrossProjects);

function wrapperFor(search: string) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return (
      <MemoryRouter initialEntries={[`/tasks?q=${encodeURIComponent(search)}`]}>
        {children}
      </MemoryRouter>
    );
  };
}

describe("useBoardServerSearch", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    searchMock.mockClear();
    boardSearchStore.getState().setSearching(false);
    projectStore.getState().setProjects([
      { id: "p1", name: "One", github_owner: "acme", github_repo: "one" },
    ] as never);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("debounces before calling the backend search", () => {
    renderHook(() => useBoardServerSearch(), { wrapper: wrapperFor("auth") });

    // Nothing fires before the debounce window elapses.
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS - 1);
    expect(searchMock).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(searchMock).toHaveBeenCalledTimes(1);
    expect(searchMock).toHaveBeenCalledWith(["acme/one"], "auth");
  });

  it("does not search for an empty query", () => {
    renderHook(() => useBoardServerSearch(), { wrapper: wrapperFor("") });
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS * 2);
    expect(searchMock).not.toHaveBeenCalled();
  });

  it("is a no-op when disabled (controlled board mode)", () => {
    renderHook(() => useBoardServerSearch({ enabled: false }), {
      wrapper: wrapperFor("auth"),
    });
    vi.advanceTimersByTime(SEARCH_DEBOUNCE_MS * 2);
    expect(searchMock).not.toHaveBeenCalled();
  });
});
