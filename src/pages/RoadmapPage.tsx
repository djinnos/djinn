import { RoadmapView } from '@/components/RoadmapView';

export function RoadmapPage() {
  return (
    <div className="flex h-full flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Roadmap</h1>
        <p className="text-muted-foreground mt-1">
          View your project timeline and milestones
        </p>
      </div>
      
      <div className="flex-1 overflow-auto rounded-lg border border-border bg-card/50">
        <RoadmapView />
      </div>
    </div>
  );
}
