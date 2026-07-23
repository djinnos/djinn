/**
 * RepositoriesSectionTabs — the shared tab strip that ties the
 * `/repositories` and `/images` routes into one section ("hub").
 *
 * Both pages render this at the top; clicking a tab navigates between the two
 * routes so deep links keep working. It's a navigation control, not a content
 * switcher, so it drives `react-router` rather than base-ui's tab panels.
 */
import { useNavigate } from "react-router-dom";

import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

export type RepositoriesSection = "repositories" | "images";

const ROUTE_BY_SECTION: Record<RepositoriesSection, string> = {
  repositories: "/repositories",
  images: "/images",
};

export function RepositoriesSectionTabs({ active }: { active: RepositoriesSection }) {
  const navigate = useNavigate();

  return (
    <Tabs
      value={active}
      onValueChange={(value) => {
        const next = value as RepositoriesSection;
        if (next !== active) navigate(ROUTE_BY_SECTION[next]);
      }}
    >
      <TabsList>
        <TabsTrigger value="repositories">Repositories</TabsTrigger>
        <TabsTrigger value="images">Images</TabsTrigger>
      </TabsList>
    </Tabs>
  );
}
