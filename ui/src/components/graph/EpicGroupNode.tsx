import type { NodeProps } from "@xyflow/react";
import { memo } from "react";

interface EpicGroupData {
  label: string;
  epicColor: string;
  emoji: string;
  status?: string;
  [key: string]: unknown;
}

const hexToRgba = (hex: string, opacity: number): string => {
  const h = hex.replace("#", "");
  const r = parseInt(h.substring(0, 2), 16);
  const g = parseInt(h.substring(2, 4), 16);
  const b = parseInt(h.substring(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${opacity})`;
};

const EpicGroupNode = memo(({ data }: NodeProps) => {
  const d = data as EpicGroupData;
  const isClosed = d.status === "closed";
  const color = isClosed ? "#6b7280" : d.epicColor;

  return (
    <div
      className="rounded-lg border-2"
      style={{
        borderColor: hexToRgba(color, 0.3),
        backgroundColor: hexToRgba(color, 0.08),
        width: "100%",
        height: "100%",
      }}
    >
      <div
        className="flex items-center gap-2 rounded-t-md px-4 py-2"
        style={{
          backgroundColor: hexToRgba(color, 0.15),
        }}
      >
        <span className="text-base">{d.emoji}</span>
        <span
          className="text-xs font-bold uppercase tracking-widest"
          style={{ color }}
        >
          {d.label}
          {isClosed && " (closed)"}
        </span>
      </div>
    </div>
  );
});

EpicGroupNode.displayName = "EpicGroupNode";

export default EpicGroupNode;
