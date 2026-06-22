import { useId, useRef, useState } from "react";
import { LayoutTable01Icon } from "@hugeicons/core-free-icons";

import { cn } from "@/lib/utils";

import { ProposalBlocks } from "./ProposalBlocks";
import { ContainerFallback } from "./ContainerFallback";
import { parseTabsAttr } from "./containerAttr";
import type { BlockProps } from "./types";

/**
 * Tabs — a recursive container block. The `tabs` attribute carries a JSON array
 * (`tabs={[{ "label", "body" }, ...]}`) where each entry's `body` is a normal
 * proposal-MDX string rendered RECURSIVELY through the shared
 * {@link ProposalBlocks} renderer (so a tab can itself contain a `<Callout/>`,
 * inline markdown, or — depth-capped — another `<Tabs/>`).
 *
 * The tab strip is a proper ARIA `tablist`: arrow keys move focus and activate
 * a tab, Home/End jump to the first/last tab, and the active tab uses djinn's
 * violet accent. The first tab is shown by default.
 *
 * If the `tabs` attribute is missing or not valid JSON the block renders a calm
 * fallback (its children markdown, else a muted notice plus the raw attribute)
 * — it never throws or blanks.
 */
export function TabsBlock({ attributes, children }: BlockProps) {
  const tabs = parseTabsAttr(attributes.tabs);
  const baseId = useId();
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const [active, setActive] = useState(0);

  if (!tabs) {
    return (
      <ContainerFallback
        kind="tabs"
        rawAttribute={attributes.tabs}
        icon={LayoutTable01Icon}
      >
        {children}
      </ContainerFallback>
    );
  }

  const activeIndex = Math.min(active, tabs.length - 1);

  const focusTab = (index: number) => {
    const clamped = (index + tabs.length) % tabs.length;
    setActive(clamped);
    tabRefs.current[clamped]?.focus();
  };

  const onKeyDown = (event: React.KeyboardEvent, index: number) => {
    switch (event.key) {
      case "ArrowRight":
      case "ArrowDown":
        event.preventDefault();
        focusTab(index + 1);
        break;
      case "ArrowLeft":
      case "ArrowUp":
        event.preventDefault();
        focusTab(index - 1);
        break;
      case "Home":
        event.preventDefault();
        focusTab(0);
        break;
      case "End":
        event.preventDefault();
        focusTab(tabs.length - 1);
        break;
      default:
        break;
    }
  };

  // De-chromed: no card. Just an underlined tab strip sitting on a hairline,
  // with the panel content flowing straight into the document below it.
  return (
    <div>
      <div className="flex items-center gap-2 border-b border-border/70">
        <div
          role="tablist"
          aria-label="Tabs"
          className="flex min-w-0 flex-nowrap gap-1 overflow-x-auto overflow-y-hidden"
        >
          {tabs.map((tab, index) => {
            const selected = index === activeIndex;
            return (
              <button
                key={`${baseId}-tab-${index}`}
                ref={(el) => {
                  tabRefs.current[index] = el;
                }}
                type="button"
                role="tab"
                id={`${baseId}-tab-${index}`}
                aria-selected={selected}
                aria-controls={`${baseId}-panel-${index}`}
                tabIndex={selected ? 0 : -1}
                onClick={() => setActive(index)}
                onKeyDown={(event) => onKeyDown(event, index)}
                className={cn(
                  "-mb-px shrink-0 whitespace-nowrap border-b-2 px-3 pb-2.5 pt-0.5 text-sm font-medium transition-colors",
                  selected
                    ? "border-violet-400 text-violet-300"
                    : "border-transparent text-muted-foreground hover:text-foreground",
                )}
              >
                {tab.label}
              </button>
            );
          })}
        </div>
      </div>
      {tabs.map((tab, index) => (
        <div
          key={`${baseId}-panel-${index}`}
          role="tabpanel"
          id={`${baseId}-panel-${index}`}
          aria-labelledby={`${baseId}-tab-${index}`}
          hidden={index !== activeIndex}
          className="pt-4"
        >
          {index === activeIndex ? <ProposalBlocks body={tab.body} /> : null}
        </div>
      ))}
    </div>
  );
}
