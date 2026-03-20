import { useProjectStore, useProjects, useSelectedProjectId } from "@/stores/useProjectStore";

export function ProjectSelector() {
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const setSelectedProjectId = useProjectStore((state) => state.setSelectedProjectId);

  return (
    <div className="flex items-center gap-2">
      <label htmlFor="project-selector" className="text-sm text-muted-foreground">Project</label>
      <select
        id="project-selector"
        className="rounded border bg-background px-2 py-1 text-sm"
        value={selectedProjectId ?? ""}
        onChange={(e) => setSelectedProjectId(e.target.value || null)}
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
