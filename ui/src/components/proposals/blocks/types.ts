import type { ReactNode } from "react";

export interface BlockProps {
  id: string;
  attributes: Record<string, string>;
  children?: ReactNode;
}
