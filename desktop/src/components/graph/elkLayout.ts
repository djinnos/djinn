import type { Edge, Node } from "@xyflow/react";
import ELK, { type ElkExtendedEdge, type ElkNode } from "elkjs";
import type { EpicData, TaskData } from "./types";

const elk = new ELK();

const toElkPortId = (nodeId: string, handleId: string): string =>
  `${nodeId}:${handleId}`;

/**
 * Builds unpositioned ReactFlow nodes and styled edges from epic groups.
 */
export function buildGraphElements(groups: EpicData[]): {
  nodes: Node[];
  edges: Edge[];
} {
  const nodes: Node[] = [];
  const edges: Edge[] = [];

  // Build lookup maps
  const taskMap = new Map<string, TaskData>();
  const taskToGroup = new Map<string, string>();
  for (const group of groups) {
    for (const task of group.tasks) {
      taskMap.set(task.id, task);
      taskToGroup.set(task.id, group.id);
    }
  }

  // Build reverse dependency map: task → tasks it blocks
  const blocksMap = new Map<string, string[]>();
  for (const group of groups) {
    for (const task of group.tasks) {
      if (!task.dependencies) continue;
      for (const dep of task.dependencies) {
        if (!taskMap.has(dep)) continue;
        const existing = blocksMap.get(dep) || [];
        existing.push(task.id);
        blocksMap.set(dep, existing);
      }
    }
  }

  for (const group of groups) {
    // Epic group node
    nodes.push({
      id: group.id,
      type: "epicGroup",
      position: { x: 0, y: 0 },
      data: { label: group.name, epicColor: group.color, emoji: group.emoji, status: group.status },
      style: { width: 400, height: 300 },
    });

    // Task nodes
    for (const task of group.tasks) {
      const targetHandles = (task.dependencies || [])
        .filter((dep) => taskMap.has(dep))
        .map((dep) => `target-${dep}`);

      const sourceHandles = (blocksMap.get(task.id) || []).map(
        (blocked) => `source-${blocked}`,
      );

      nodes.push({
        id: task.id,
        type: "taskNode",
        position: { x: 0, y: 0 },
        parentId: group.id,
        extent: "parent" as const,
        data: {
          label: task.title,
          status: task.status,
          epicColor: group.color,
          epicName: group.name,
          epicEmoji: group.emoji,
          description: task.description || "",
          dependencies: task.dependencies || [],
          taskId: task.id,
          task: task.task,
          targetHandles,
          sourceHandles,
        },
      });

      // Dependency edges
      if (!task.dependencies) continue;
      const seen = new Set<string>();
      for (const dep of task.dependencies) {
        const edgeId = `${dep}->${task.id}`;
        if (seen.has(edgeId)) continue;
        seen.add(edgeId);

        const sourceTask = taskMap.get(dep);
        if (!sourceTask) continue;

        const isResolved =
          sourceTask.status === "closed" || sourceTask.status === "approved";

        edges.push({
          id: edgeId,
          source: dep,
          target: task.id,
          sourceHandle: `source-${task.id}`,
          targetHandle: `target-${dep}`,
          type: "smoothstep",
          animated: !isResolved,
          style: {
            stroke: isResolved
              ? "var(--muted-foreground, hsl(215 12% 55%))"
              : "var(--destructive, hsl(0 72% 51%))",
            strokeWidth: isResolved ? 1.5 : 2,
            strokeDasharray: isResolved ? undefined : "5,5",
          },
        });
      }
    }
  }

  return { nodes, edges };
}

export async function layoutWithElk(
  nodes: Node[],
  edges: Edge[],
): Promise<{ nodes: Node[]; edges: Edge[] }> {
  const groupNodes = nodes.filter((n) => n.type === "epicGroup");
  const taskNodes = nodes.filter((n) => n.type === "taskNode");

  const taskToEpic = new Map<string, string>();
  for (const t of taskNodes) {
    if (t.parentId) taskToEpic.set(t.id, t.parentId);
  }

  const taskHandles = new Map<
    string,
    { targetHandles: string[]; sourceHandles: string[] }
  >();
  for (const t of taskNodes) {
    const data = t.data as {
      targetHandles?: string[];
      sourceHandles?: string[];
    };
    taskHandles.set(t.id, {
      targetHandles: data.targetHandles || [],
      sourceHandles: data.sourceHandles || [],
    });
  }

  const allElkEdges: ElkExtendedEdge[] = edges.map((e) => {
    const sourceEpic = taskToEpic.get(e.source);
    const targetEpic = taskToEpic.get(e.target);
    const isIntraEpic = sourceEpic && targetEpic && sourceEpic === targetEpic;

    return {
      id: e.id,
      sources: [
        e.sourceHandle ? toElkPortId(e.source, e.sourceHandle) : e.source,
      ],
      targets: [
        e.targetHandle ? toElkPortId(e.target, e.targetHandle) : e.target,
      ],
      layoutOptions: {
        "elk.layered.priority.direction": isIntraEpic ? "10" : "1",
      },
    };
  });

  const elkChildren: ElkNode[] = groupNodes.map((epic) => {
    const children = taskNodes
      .filter((t) => t.parentId === epic.id)
      .map((t) => {
        const handles = taskHandles.get(t.id) || {
          targetHandles: [],
          sourceHandles: [],
        };

        const ports = [
          ...handles.targetHandles.map((id) => ({
            id: toElkPortId(t.id, id),
            properties: { "port.side": "WEST" },
            width: 8,
            height: 8,
          })),
          ...handles.sourceHandles.map((id) => ({
            id: toElkPortId(t.id, id),
            properties: { "port.side": "EAST" },
            width: 8,
            height: 8,
          })),
        ];

        return {
          id: t.id,
          width: 260,
          height: 140,
          ports,
          layoutOptions: {
            "elk.portConstraints": "FIXED_SIDE",
          },
        };
      });

    return {
      id: epic.id,
      children,
      edges: [],
      layoutOptions: {
        "elk.algorithm": "layered",
        "elk.direction": "RIGHT",
        "elk.spacing.nodeNode": "25",
        "elk.layered.spacing.nodeNodeBetweenLayers": "50",
        "elk.padding": "[top=55,left=25,bottom=25,right=25]",
        "elk.portConstraints": "FIXED_SIDE",
        "elk.edgeRouting": "ORTHOGONAL",
      },
    };
  });

  const graph: ElkNode = {
    id: "root",
    children: elkChildren,
    edges: allElkEdges,
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": "RIGHT",
      "elk.hierarchyHandling": "INCLUDE_CHILDREN",
      "elk.spacing.nodeNode": "40",
      "elk.layered.spacing.nodeNodeBetweenLayers": "80",
      "elk.portConstraints": "FIXED_SIDE",
      "elk.layered.crossingMinimization.strategy": "LAYER_SWEEP",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.layered.nodePlacement.strategy": "NETWORK_SIMPLEX",
    },
  };

  const layouted = await elk.layout(graph);

  const positionedNodes = nodes.map((node) => {
    if (node.type === "epicGroup") {
      const elkEpic = layouted.children?.find((c) => c.id === node.id);
      if (elkEpic) {
        return {
          ...node,
          position: { x: elkEpic.x ?? 0, y: elkEpic.y ?? 0 },
          style: {
            width: elkEpic.width ?? 400,
            height: elkEpic.height ?? 300,
          },
        };
      }
    } else if (node.parentId) {
      const elkEpic = layouted.children?.find((c) => c.id === node.parentId);
      const elkTask = elkEpic?.children?.find((c) => c.id === node.id);
      if (elkTask) {
        return {
          ...node,
          position: { x: elkTask.x ?? 0, y: elkTask.y ?? 0 },
        };
      }
    }
    return node;
  });

  return { nodes: positionedNodes, edges };
}
