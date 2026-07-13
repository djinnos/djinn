import {
  applyEdgeChanges,
  applyNodeChanges,
  Background,
  BackgroundVariant,
  Controls,
  type Edge,
  type EdgeChange,
  MiniMap,
  type Node,
  type NodeChange,
  ReactFlow,
} from "@xyflow/react";
import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import { useNavigate } from "react-router-dom";
import "@xyflow/react/dist/style.css";

import { detectStructuralChanges, fingerprintEpics } from "./change-detection";
import EpicGroupNode from "./EpicGroupNode";
import { buildGraphElements, layoutWithElk } from "./elkLayout";
import {
  GraphHighlightContext,
  type GraphHighlight,
} from "./highlightContext";
import TaskNode from "./TaskNode";
import type { EpicData } from "./types";

type GraphReducerState = { nodes: Node[]; edges: Edge[]; isLoading: boolean };
type GraphAction =
  | { type: "loading" }
  | { type: "clear" }
  | { type: "set"; nodes: Node[]; edges: Edge[] }
  | { type: "nodesChange"; changes: NodeChange[] }
  | { type: "edgesChange"; changes: EdgeChange[] }
  | { type: "updateNodes"; updater: (prev: Node[]) => Node[] };

function graphReducer(
  state: GraphReducerState,
  action: GraphAction,
): GraphReducerState {
  switch (action.type) {
    case "loading":
      return { ...state, isLoading: true };
    case "clear":
      return { nodes: [], edges: [], isLoading: false };
    case "set":
      return { nodes: action.nodes, edges: action.edges, isLoading: false };
    case "nodesChange":
      return { ...state, nodes: applyNodeChanges(action.changes, state.nodes) };
    case "edgesChange":
      return { ...state, edges: applyEdgeChanges(action.changes, state.edges) };
    case "updateNodes":
      return { ...state, nodes: action.updater(state.nodes) };
  }
}

const nodeTypes = {
  taskNode: TaskNode,
  epicGroup: EpicGroupNode,
};

interface DependencyGraphProps {
  epics: EpicData[];
  /** Tasks matching the active search — drawn with a ring. */
  highlightTaskIds?: Set<string>;
  /** Out-of-scope / non-matching tasks — drawn faded for context. */
  dimTaskIds?: Set<string>;
}

const EMPTY_IDS = new Set<string>();

const DependencyGraph = ({
  epics,
  highlightTaskIds = EMPTY_IDS,
  dimTaskIds = EMPTY_IDS,
}: DependencyGraphProps) => {
  const navigate = useNavigate();
  const highlight = useMemo<GraphHighlight>(
    () => ({ highlightTaskIds, dimTaskIds }),
    [highlightTaskIds, dimTaskIds],
  );
  const [graphState, dispatch] = useReducer(graphReducer, {
    nodes: [],
    edges: [],
    isLoading: true,
  });
  const { nodes, edges, isLoading } = graphState;

  const onNodesChange = useCallback(
    (changes: NodeChange[]) => dispatch({ type: "nodesChange", changes }),
    [],
  );
  const onEdgesChange = useCallback(
    (changes: EdgeChange[]) => dispatch({ type: "edgesChange", changes }),
    [],
  );

  const prevEpicsRef = useRef<EpicData[]>([]);

  const runLayout = useCallback(async (epicsToLayout: EpicData[]) => {
    dispatch({ type: "loading" });
    const { nodes: rawNodes, edges: rawEdges } =
      buildGraphElements(epicsToLayout);
    const { nodes: layoutedNodes, edges: layoutedEdges } = await layoutWithElk(
      rawNodes,
      rawEdges,
    );
    dispatch({ type: "set", nodes: layoutedNodes, edges: layoutedEdges });
  }, []);

  const epicsRef = useRef<EpicData[]>(epics);

  const updateNodeData = useCallback((changedTaskIds: string[]) => {
    const taskMap = new Map<
      string,
      { status: string; title: string; description?: string }
    >();
    for (const epic of epicsRef.current) {
      for (const task of epic.tasks) {
        if (changedTaskIds.includes(task.id)) {
          taskMap.set(task.id, {
            status: task.status,
            title: task.title,
            description: task.description,
          });
        }
      }
    }

    dispatch({
      type: "updateNodes",
      updater: (prevNodes: Node[]) =>
        prevNodes.map((node) => {
          if (node.type === "taskNode" && taskMap.has(node.id)) {
            const updatedTask = taskMap.get(node.id)!;
            return {
              ...node,
              data: {
                ...node.data,
                label: updatedTask.title,
                status: updatedTask.status,
                description: updatedTask.description || "",
              },
            };
          }
          return node;
        }),
    });
  }, []);

  const runLayoutRef = useRef(runLayout);
  const updateNodeDataRef = useRef(updateNodeData);

  // Keep the "latest value" refs current from an effect (after commit) rather
  // than mutating them during render. This effect is declared before the main
  // layout effect below, so it commits first and the reads there stay fresh.
  useEffect(() => {
    epicsRef.current = epics;
    runLayoutRef.current = runLayout;
    updateNodeDataRef.current = updateNodeData;
  });

  const epicsFp = useMemo(() => fingerprintEpics(epics), [epics]);

  useEffect(() => {
    void epicsFp;
    const epics = epicsRef.current;

    if (prevEpicsRef.current.length === 0 && epics.length > 0) {
      runLayoutRef.current(epics);
      prevEpicsRef.current = epics;
      return;
    }

    if (epics.length === 0) {
      dispatch({ type: "clear" });
      prevEpicsRef.current = [];
      return;
    }

    const timeoutId = setTimeout(() => {
      const epics = epicsRef.current;
      const { isStructuralChange, changedTaskIds } = detectStructuralChanges(
        prevEpicsRef.current,
        epics,
      );

      if (isStructuralChange) {
        runLayoutRef.current(epics);
      } else if (changedTaskIds.length > 0) {
        updateNodeDataRef.current(changedTaskIds);
      }

      prevEpicsRef.current = epics;
    }, 100);

    return () => clearTimeout(timeoutId);
  }, [epicsFp]);

  const onNodeClick = useCallback(
    (_: React.MouseEvent, node: Node) => {
      if (node.type !== "taskNode") return;
      const d = node.data as Record<string, unknown>;
      navigate(`/task/${d.taskId as string}`);
    },
    [navigate],
  );

  if (isLoading) {
    return (
      <div className="flex h-full w-full items-center justify-center">
        <div className="flex flex-col items-center gap-3">
          <div className="h-8 w-8 animate-spin rounded-full border-2 border-primary border-t-transparent" />
          <span className="text-sm text-muted-foreground font-mono">
            Computing layout...
          </span>
        </div>
      </div>
    );
  }

  return (
    <GraphHighlightContext.Provider value={highlight}>
      <div className="h-full w-full">
        <ReactFlow
          nodes={nodes}
          edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={onNodeClick}
        nodeTypes={nodeTypes}
        nodesConnectable={false}
        nodesDraggable={false}
        fitView
        fitViewOptions={{ padding: 0.2 }}
        minZoom={0.1}
        maxZoom={2}
        proOptions={{ hideAttribution: true }}
      >
        <Background
          variant={BackgroundVariant.Dots}
          gap={20}
          size={1}
          className="!bg-background"
        />
        <Controls className="!rounded-lg !border-border !bg-card !shadow-xl [&>button]:!border-border [&>button]:!bg-card [&>button]:!text-foreground [&>button:hover]:!bg-secondary" />
        <MiniMap
          className="!rounded-lg !border-border !bg-card"
          nodeColor={(node) => {
            if (node.type === "epicGroup") {
              return (
                (node.data as { epicColor?: string }).epicColor || "#6366f1"
              );
            }
            return "var(--muted, #333)";
          }}
          maskColor="rgba(0, 0, 0, 0.7)"
        />
        </ReactFlow>
      </div>
    </GraphHighlightContext.Provider>
  );
};

export default DependencyGraph;
