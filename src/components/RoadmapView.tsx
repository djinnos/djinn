/**
 * RoadmapView page component - Lists all epics with their progress
 * 
 * Fetches epics from epicStore and tasks from taskStore.
 * Displays EpicCards sorted by priority (P0 first) then creation order.
 */

import { useMemo } from "react";
import { EpicCard } from "./EpicCard";
import { useAllEpics } from "@/stores/useEpicStore";
import type { Epic } from "@/types";

/**
 * Sort epics by priority (P0 < P1 < P2 < P3) then by creation date (newest first)
 */
function sortEpics(epics: Epic[]): Epic[] {
  const priorityOrder: Record<Epic["priority"], number> = {
    P0: 0,
    P1: 1,
    P2: 2,
    P3: 3,
  };

  return [...epics].sort((a, b) => {
    // First sort by priority
    const priorityDiff = priorityOrder[a.priority] - priorityOrder[b.priority];
    if (priorityDiff !== 0) {
      return priorityDiff;
    }
    // Then by creation date (newest first)
    return new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime();
  });
}

/**
 * Get a deterministic emoji for an epic based on its ID
 * This ensures the same epic always gets the same emoji
 */
function getEpicEmoji(epicId: string): string {
  const emojis = ["🚀", "🎯", "⭐", "🔥", "💎", "🎨", "⚡", "🔧", "📊", "🎪", "🏆", "🌟"];
  let hash = 0;
  for (let i = 0; i < epicId.length; i++) {
    const char = epicId.charCodeAt(i);
    hash = ((hash << 5) - hash) + char;
    hash = hash & hash;
  }
  return emojis[Math.abs(hash) % emojis.length];
}

export function RoadmapView() {
  const epics = useAllEpics();
  const sortedEpics = useMemo(() => sortEpics(epics), [epics]);

  if (epics.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center p-8 text-center">
        <div className="mb-4 text-4xl" role="img" aria-label="empty roadmap">
          🗺️
        </div>
        <h2 className="mb-2 text-lg font-semibold">No Epics Yet</h2>
        <p className="text-sm text-muted-foreground">
          Create your first epic to start tracking progress on the roadmap.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Roadmap</h1>
        <span className="text-sm text-muted-foreground">
          {epics.length} epic{epics.length !== 1 ? "s" : ""}
        </span>
      </div>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
        {sortedEpics.map((epic) => (
          <EpicCard 
            key={epic.id} 
            epic={epic} 
            emoji={getEpicEmoji(epic.id)}
          />
        ))}
      </div>
    </div>
  );
}
