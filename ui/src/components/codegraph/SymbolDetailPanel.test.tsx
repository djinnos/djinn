import { beforeEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { SymbolDetailPanel } from "./SymbolDetailPanel";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { SymbolContext } from "@/api/codeGraph";

function ctx(overrides?: Partial<SymbolContext>): SymbolContext {
  return {
    symbol: {
      uid: "scip:rust . crate . :: foo()",
      name: "foo",
      kind: "function",
      file_path: "src/lib/foo.rs",
      start_line: 10,
      end_line: 25,
      content: null,
      method_metadata: null,
    },
    incoming: {},
    outgoing: {},
    processes: [],
    ...overrides,
  };
}

describe("SymbolDetailPanel", () => {
  beforeEach(() => {
    useCodeGraphStore.getState().reset();
  });

  it("returns null when no selection is set", () => {
    const { container } = render(
      <SymbolDetailPanel projectId="proj" injectedContext={null} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders the symbol header from injected context", () => {
    useCodeGraphStore.getState().setSelection("scip:rust . crate . :: foo()");
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          symbol: {
            uid: "scip:rust . crate . :: foo()",
            name: "foo",
            kind: "function",
            file_path: "src/lib/foo.rs",
            start_line: 10,
            end_line: 25,
            content: null,
            method_metadata: null,
          },
        })}
      />,
    );
    expect(screen.getByText("foo")).toBeInTheDocument();
    expect(screen.getByText("function")).toBeInTheDocument();
    // file_path:start-end (truncatePathLeft may not chop; just check digits exist)
    expect(screen.getByText(/10-25/)).toBeInTheDocument();
  });

  it("renders incoming category buckets", () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          incoming: {
            calls: [
              {
                uid: "caller-1",
                name: "caller_one",
                kind: "function",
                file_path: "src/x.rs",
                confidence: 0.9,
              },
            ],
          },
        })}
      />,
    );
    expect(screen.getByText("Calls")).toBeInTheDocument();
    expect(screen.getByText("caller_one")).toBeInTheDocument();
  });

  it("clicking a related symbol updates the selection", async () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          outgoing: {
            calls: [
              {
                uid: "callee-1",
                name: "do_thing",
                kind: "function",
                file_path: null,
                confidence: 0.8,
              },
            ],
          },
        })}
      />,
    );
    await userEvent.click(screen.getByText("do_thing"));
    expect(useCodeGraphStore.getState().selectionId).toBe("callee-1");
  });

  it("close button clears the selection", async () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(<SymbolDetailPanel projectId="proj" injectedContext={ctx()} />);
    await userEvent.click(screen.getByLabelText("Close detail panel"));
    expect(useCodeGraphStore.getState().selectionId).toBeNull();
  });

  it("renders method metadata when present", () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          symbol: {
            uid: "foo",
            name: "do_async",
            kind: "method",
            file_path: "src/lib.rs",
            start_line: 1,
            end_line: 5,
            content: null,
            method_metadata: {
              visibility: "pub",
              is_async: true,
              params: [
                { name: "value", type_name: "u64", default_value: null },
              ],
              return_type: "Result<()>",
              annotations: [],
            },
          },
        })}
      />,
    );
    expect(screen.getByText("pub")).toBeInTheDocument();
    expect(screen.getByText("async")).toBeInTheDocument();
    expect(screen.getByText("value")).toBeInTheDocument();
    expect(screen.getByText(/u64/)).toBeInTheDocument();
    expect(screen.getByText("Result<()>")).toBeInTheDocument();
  });

  it("does not render the legacy standalone blast radius CTA", () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(<SymbolDetailPanel projectId="proj" injectedContext={ctx()} />);
    expect(screen.queryByText("Show blast radius")).not.toBeInTheDocument();
    expect(screen.getByText("DOI focus")).toBeInTheDocument();
  });

  it("labels dependency directions and omits containment buckets", () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          incoming: {
            calls: [
              {
                uid: "caller-1",
                name: "caller_one",
                kind: "function",
                file_path: null,
                confidence: 0.9,
              },
            ],
            contains: [
              {
                uid: "container",
                name: "container",
                kind: "class",
                file_path: null,
                confidence: 1,
              },
            ],
          },
          outgoing: {
            references: [
              {
                uid: "dep-1",
                name: "dependency",
                kind: "function",
                file_path: null,
                confidence: 0.8,
              },
            ],
          },
        })}
      />,
    );

    expect(screen.getAllByText("Dependents/Impact")[0]).toBeInTheDocument();
    expect(screen.getAllByText("Dependencies")[0]).toBeInTheDocument();
    expect(screen.getByText("caller_one")).toBeInTheDocument();
    expect(screen.getByText("dependency")).toBeInTheDocument();
    expect(screen.queryByText("Contains")).not.toBeInTheDocument();
    expect(screen.queryByText("container")).not.toBeInTheDocument();
  });

  it("renders trait-dispatch Dependents/Impact and Dependencies for a trait method with in-repo callers", () => {
    useCodeGraphStore.getState().setSelection(
      "symbol:runtime_bridge.rs::list_taskrun_jobs",
    );
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          symbol: {
            uid: "symbol:runtime_bridge.rs::list_taskrun_jobs",
            name: "list_taskrun_jobs",
            kind: "method",
            file_path:
              "server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs",
            start_line: 137,
            end_line: 137,
            content: null,
            method_metadata: null,
          },
          incoming: {
            calls: [
              {
                uid: "symbol:health.rs::reap_orphaned_taskrun_jobs",
                name: "reap_orphaned_taskrun_jobs",
                kind: "function",
                file_path:
                  "server/crates/djinn-agent/src/actors/coordinator/health.rs",
                confidence: 0.7,
                confidence_tier: "inferred",
                confidence_reason: "trait-dispatch-call",
              },
            ],
          },
          outgoing: {
            implements: [
              {
                uid: "symbol:app_state.rs::list_taskrun_jobs",
                name: "list_taskrun_jobs",
                kind: "method",
                file_path: "server/src/mcp_bridge/mod.rs",
                confidence: 0.9,
                confidence_tier: "extracted",
              },
            ],
          },
        })}
      />,
    );

    // Dependents/Impact section shows the trait-dispatch caller
    const dependentsHeaders = screen.getAllByText("Dependents/Impact");
    expect(dependentsHeaders.length).toBeGreaterThanOrEqual(1);
    expect(dependentsHeaders[0]).toBeInTheDocument();
    expect(screen.getByText("Calls")).toBeInTheDocument();
    expect(
      screen.getByText("reap_orphaned_taskrun_jobs"),
    ).toBeInTheDocument();

    // Dependencies section shows the concrete impl via Implements
    const dependenciesHeaders = screen.getAllByText("Dependencies");
    expect(dependenciesHeaders.length).toBeGreaterThanOrEqual(1);
    expect(dependenciesHeaders[0]).toBeInTheDocument();
    expect(screen.getByText("Implements")).toBeInTheDocument();
    const listTaskrunJobs = screen.getAllByText("list_taskrun_jobs");
    expect(listTaskrunJobs.length).toBeGreaterThanOrEqual(1);
    expect(listTaskrunJobs[0]).toBeInTheDocument();

    // The caller file path truncates correctly
    expect(
      screen.getByTitle("symbol:health.rs::reap_orphaned_taskrun_jobs"),
    ).toBeInTheDocument();
  });

  it("parses backend-shaped trait-dispatch context with all optional confidence fields", () => {
    useCodeGraphStore.getState().setSelection(
      "symbol:runtime_bridge.rs::list_taskrun_jobs",
    );
    render(
      <SymbolDetailPanel
        projectId="proj"
        injectedContext={ctx({
          symbol: {
            uid: "symbol:runtime_bridge.rs::list_taskrun_jobs",
            name: "list_taskrun_jobs",
            kind: "method",
            file_path:
              "server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs",
            start_line: 137,
            end_line: 137,
            content: null,
            method_metadata: null,
          },
          incoming: {
            calls: [
              {
                uid: "symbol:health.rs::reap_orphaned_taskrun_jobs",
                name: "reap_orphaned_taskrun_jobs",
                kind: "function",
                file_path:
                  "server/crates/djinn-agent/src/actors/coordinator/health.rs",
                confidence: 0.7,
                confidence_tier: "inferred",
                confidence_reason: "trait-dispatch-call",
              },
            ],
          },
          outgoing: {
            implements: [
              {
                uid: "symbol:app_state.rs::list_taskrun_jobs",
                name: "list_taskrun_jobs",
                kind: "method",
                file_path: "server/src/mcp_bridge/mod.rs",
                confidence: 0.9,
                confidence_tier: "extracted",
              },
            ],
          },
        })}
      />,
    );

    // Non-empty Dependents/Impact renders
    const dependentsHeaders2 = screen.getAllByText("Dependents/Impact");
    expect(dependentsHeaders2.length).toBeGreaterThanOrEqual(1);
    expect(dependentsHeaders2[0]).toBeInTheDocument();
    expect(screen.getByText("Calls")).toBeInTheDocument();
    expect(
      screen.getByText("reap_orphaned_taskrun_jobs"),
    ).toBeInTheDocument();

    // Non-empty Dependencies renders
    const dependenciesHeaders2 = screen.getAllByText("Dependencies");
    expect(dependenciesHeaders2.length).toBeGreaterThanOrEqual(1);
    expect(dependenciesHeaders2[0]).toBeInTheDocument();
    expect(screen.getByText("Implements")).toBeInTheDocument();
  });

  it("renders a placeholder when no edges are present", () => {
    useCodeGraphStore.getState().setSelection("foo");
    render(<SymbolDetailPanel projectId="proj" injectedContext={ctx()} />);
    const placeholders = screen.getAllByText("No edges.");
    // Both incoming and outgoing sections collapse to placeholder.
    expect(placeholders.length).toBe(2);
  });
});
