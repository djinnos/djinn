import { useState } from "react";

import { ProjectSelector } from "./ProjectSelector";

const mockProjects = [
  { id: "p1", name: "Desktop App" },
  { id: "p2", name: "Server" },
  { id: "p3", name: "Website" },
];

export default {
  title: "Components/ProjectSelector",
  component: ProjectSelector,
};

export function Default() {
  const [selectedId, setSelectedId] = useState<string | null>(mockProjects[0]?.id ?? null);

  return <ProjectSelector projects={mockProjects} selectedId={selectedId} onSelect={setSelectedId} />;
}
