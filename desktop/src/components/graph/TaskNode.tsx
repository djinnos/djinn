import type { Task } from "@/api/types";
import { Handle, type NodeProps, Position } from "@xyflow/react";
import { memo } from "react";
import { TaskCard } from "@/components/TaskCard";

interface TaskNodeData {
  task: Task;
  epicColor: string;
  epicName: string;
  epicEmoji: string;
  targetHandles: string[];
  sourceHandles: string[];
  [key: string]: unknown;
}

/**
 * Custom ReactFlow node that wraps the existing TaskCard with connection handles.
 *
 * - Left handles (target) for incoming dependency edges
 * - Right handles (source) for outgoing dependency edges
 *
 * Width 260px to match ELK declared dimensions.
 */
const TaskNode = memo(({ data }: NodeProps) => {
  const d = data as TaskNodeData;
  const targetHandles = d.targetHandles ?? [];
  const sourceHandles = d.sourceHandles ?? [];

  return (
    <div className="relative w-[260px]">
      {/* Target handles (left side) */}
      {targetHandles.length > 0 ? (
        targetHandles.map((handleId, index) => (
          <Handle
            key={handleId}
            id={handleId}
            type="target"
            position={Position.Left}
            className="!h-2 !w-2 !border-2 !border-secondary !bg-muted-foreground"
            style={{
              top: `${((index + 1) / (targetHandles.length + 1)) * 100}%`,
            }}
          />
        ))
      ) : (
        <Handle
          type="target"
          position={Position.Left}
          className="!h-2 !w-2 !border-2 !border-secondary !bg-muted-foreground"
        />
      )}

      <TaskCard task={d.task} />

      {/* Source handles (right side) */}
      {sourceHandles.length > 0 ? (
        sourceHandles.map((handleId, index) => (
          <Handle
            key={handleId}
            id={handleId}
            type="source"
            position={Position.Right}
            className="!h-2 !w-2 !border-2 !border-secondary !bg-muted-foreground"
            style={{
              top: `${((index + 1) / (sourceHandles.length + 1)) * 100}%`,
            }}
          />
        ))
      ) : (
        <Handle
          type="source"
          position={Position.Right}
          className="!h-2 !w-2 !border-2 !border-secondary !bg-muted-foreground"
        />
      )}
    </div>
  );
});

TaskNode.displayName = "TaskNode";
export default TaskNode;
