/**
 * Shared layout for the Tasks board and the Dependencies graph: a single
 * persistent filter header on top, the active view rendered below via the
 * router Outlet. Keeping the header in the layout means filter state (and the
 * header's own input) survive toggling between the two views.
 */
import { Outlet } from "react-router-dom";
import { BoardFilterHeader } from "./BoardFilterHeader";

export function BoardLayout() {
  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden pt-5">
      <BoardFilterHeader />
      <div className="min-h-0 flex-1">
        <Outlet />
      </div>
    </div>
  );
}
