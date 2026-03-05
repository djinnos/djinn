import * as React from "react";
import { Search } from "lucide-react";

import { cn } from "@/lib/utils";
import {
  AlertDialog,
  AlertDialogContent,
} from "@/components/ui/alert-dialog";

type CommandContextValue = {
  query: string;
  setQuery: (query: string) => void;
};

const CommandContext = React.createContext<CommandContextValue | null>(null);

function useCommandContext() {
  const context = React.useContext(CommandContext);
  if (!context) {
    throw new Error("Command components must be used within <Command>");
  }
  return context;
}

function Command({ className, children, ...props }: React.ComponentProps<"div">) {
  const [query, setQuery] = React.useState("");

  return (
    <CommandContext.Provider value={{ query, setQuery }}>
      <div
        data-slot="command"
        className={cn(
          "bg-popover text-popover-foreground flex h-full w-full flex-col overflow-hidden rounded-md",
          className,
        )}
        {...props}
      >
        {children}
      </div>
    </CommandContext.Provider>
  );
}

function CommandDialog({
  children,
  open,
  onOpenChange,
}: {
  children: React.ReactNode;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogContent className="overflow-hidden p-0 shadow-lg" size="sm">
        <Command className="[&_[data-slot=command-group-heading]]:text-muted-foreground [&_[data-slot=command-group-heading]]:px-2 [&_[data-slot=command-group-heading]]:py-1.5 [&_[data-slot=command-group-heading]]:text-xs [&_[data-slot=command-group-items]]:px-2 [&_[data-slot=command-group]]:px-2 [&_[data-slot=command-input-wrapper]_svg]:h-5 [&_[data-slot=command-input-wrapper]_svg]:w-5 [&_[data-slot=command-input]]:h-12 [&_[data-slot=command-item]]:px-2 [&_[data-slot=command-item]]:py-3 [&_[data-slot=command-item]_svg]:h-5 [&_[data-slot=command-item]_svg]:w-5">
          {children}
        </Command>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function CommandInput({ className, ...props }: React.ComponentProps<"input">) {
  const { query, setQuery } = useCommandContext();

  return (
    <div data-slot="command-input-wrapper" className="flex h-9 items-center gap-2 border-b px-3">
      <Search className="size-4 shrink-0 opacity-50" />
      <input
        data-slot="command-input"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        className={cn(
          "placeholder:text-muted-foreground flex h-10 w-full rounded-md bg-transparent py-3 text-sm outline-none disabled:cursor-not-allowed disabled:opacity-50",
          className,
        )}
        {...props}
      />
    </div>
  );
}

function CommandList({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="command-list"
      className={cn("max-h-[300px] overflow-y-auto overflow-x-hidden", className)}
      {...props}
    />
  );
}

function CommandEmpty({ className, ...props }: React.ComponentProps<"div">) {
  const { query } = useCommandContext();
  if (!query) return null;

  return (
    <div
      data-slot="command-empty"
      className={cn("py-6 text-center text-sm", className)}
      {...props}
    />
  );
}

function CommandGroup({ className, children, heading, ...props }: React.ComponentProps<"div"> & { heading?: React.ReactNode }) {
  return (
    <div
      data-slot="command-group"
      className={cn(
        "text-foreground overflow-hidden p-1",
        className,
      )}
      {...props}
    >
      {heading ? <div data-slot="command-group-heading" className="px-2 py-1.5 text-xs font-medium text-muted-foreground">{heading}</div> : null}
      <div data-slot="command-group-items">{children}</div>
    </div>
  );
}

function CommandSeparator({ className, ...props }: React.ComponentProps<"div">) {
  return <div data-slot="command-separator" className={cn("bg-border -mx-1 h-px", className)} {...props} />;
}

function CommandItem({
  className,
  children,
  onSelect,
  ...props
}: React.ComponentProps<"button"> & { onSelect?: () => void }) {
  const { query } = useCommandContext();
  const text = typeof children === "string" ? children : "";
  const visible = !query || text.toLowerCase().includes(query.toLowerCase());

  if (!visible) return null;

  return (
    <button
      type="button"
      data-slot="command-item"
      className={cn(
        "relative flex w-full cursor-default items-center gap-2 rounded-sm px-2 py-1.5 text-sm outline-none select-none hover:bg-accent hover:text-accent-foreground",
        className,
      )}
      onClick={onSelect}
      {...props}
    >
      {children}
    </button>
  );
}

function CommandShortcut({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      data-slot="command-shortcut"
      className={cn("text-muted-foreground ml-auto text-xs tracking-widest", className)}
      {...props}
    />
  );
}

export {
  Command,
  CommandDialog,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
  CommandShortcut,
  CommandSeparator,
};
