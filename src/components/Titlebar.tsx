import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Cancel01Icon,
  Minus01Icon,
  Square01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

const appWindow = getCurrentWindow();

function TitlebarButton({
  onClick,
  label,
  children,
  variant = "default",
}: {
  onClick: () => void;
  label: string;
  children: React.ReactNode;
  variant?: "default" | "close";
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      className={`flex h-full w-10 items-center justify-center transition-colors ${
        variant === "close"
          ? "hover:bg-red-500/90 hover:text-white"
          : "hover:bg-muted"
      }`}
    >
      {children}
    </button>
  );
}

export function Titlebar() {
  return (
    <div
      data-tauri-drag-region
      className="flex h-9 select-none items-center justify-end border-b border-border/50 bg-background"
    >
      <TitlebarButton onClick={() => appWindow.minimize()} label="Minimize">
        <HugeiconsIcon icon={Minus01Icon} size={14} className="pointer-events-none" />
      </TitlebarButton>
      <TitlebarButton onClick={() => appWindow.toggleMaximize()} label="Maximize">
        <HugeiconsIcon icon={Square01Icon} size={12} className="pointer-events-none" />
      </TitlebarButton>
      <TitlebarButton onClick={() => appWindow.close()} label="Close" variant="close">
        <HugeiconsIcon icon={Cancel01Icon} size={14} className="pointer-events-none" />
      </TitlebarButton>
    </div>
  );
}
