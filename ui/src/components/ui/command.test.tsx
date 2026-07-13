import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { render, screen, userEvent } from "@/test/test-utils";

import {
  Command,
  CommandInput,
  CommandItem,
  CommandList,
} from "./command";

function ObservedCommandInput({
  onObserved,
}: {
  onObserved: (value: string) => void;
}) {
  const [observed, setObserved] = useState("");

  return (
    <Command>
      <CommandInput
        aria-label="Search commands"
        onChange={(event) => {
          const value = event.currentTarget.value;
          setObserved(value);
          onObserved(value);
        }}
      />
      <output data-testid="observed-query">{observed}</output>
      <CommandList>
        <CommandItem searchValue="alpha">Alpha</CommandItem>
        <CommandItem searchValue="beta">Beta</CommandItem>
      </CommandList>
    </Command>
  );
}

describe("CommandInput", () => {
  it("keeps its query and an external change observer in sync for every key", async () => {
    const user = userEvent.setup();
    const onObserved = vi.fn();
    render(<ObservedCommandInput onObserved={onObserved} />);

    const input = screen.getByRole("textbox", { name: "Search commands" });
    await user.type(input, "bet");

    expect(input).toHaveValue("bet");
    expect(screen.getByTestId("observed-query")).toHaveTextContent("bet");
    expect(onObserved.mock.calls.map(([value]) => value)).toEqual([
      "b",
      "be",
      "bet",
    ]);
    expect(screen.queryByRole("button", { name: "Alpha" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Beta" })).toBeInTheDocument();
  });
});
