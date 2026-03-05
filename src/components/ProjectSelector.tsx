import type { Project } from "@/types";

type ProjectSelectorProps = {
  projects: Project[];
  selectedId: string | null;
  onSelect: (projectId: string | null) => void;
};

export function ProjectSelector({ projects, selectedId, onSelect }: ProjectSelectorProps) {
  return (
    <div className="flex items-center gap-2">
      <label htmlFor="project-selector" className="text-sm text-muted-foreground">Project</label>
      <select
        id="project-selector"
        className="rounded border bg-background px-2 py-1 text-sm"
        value={selectedId ?? ""}
        onChange={(e) => onSelect(e.target.value || null)}
      >
        {projects.map((project) => (
          <option key={project.id} value={project.id}>
            {project.name}
          </option>
        ))}
      </select>
    </div>
  );
}
