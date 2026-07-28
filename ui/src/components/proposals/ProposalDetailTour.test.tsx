import { beforeEach, describe, expect, it, vi } from "vitest";

import { render, screen, userEvent, waitFor } from "@/test/test-utils";

import {
  ProposalDetailTour,
} from "./ProposalDetailTour";
import { proposalDetailTourStorageKey } from "./proposalTourStorage";

const userId = "user-tour";

function TourTargets() {
  return (
    <>
      <div data-proposal-tour="overview">Overview</div>
      <div data-proposal-tour="spec">Spec</div>
      <div data-proposal-tour="refinement">Refinement</div>
      <div data-proposal-tour="readiness">Readiness</div>
      <div data-proposal-tour="approval">Approval</div>
    </>
  );
}

describe("ProposalDetailTour", () => {
  beforeEach(() => {
    window.localStorage.clear();
    Element.prototype.scrollIntoView = vi.fn();
  });

  it("opens on the first visit, walks every feature, and persists completion", async () => {
    const user = userEvent.setup();
    const { unmount } = render(
      <>
        <TourTargets />
        <ProposalDetailTour userId={userId} />
      </>,
    );

    const dialog = await screen.findByRole("dialog", {
      name: "The proposal brief",
    });
    await waitFor(() => expect(dialog).toHaveFocus());
    await user.tab({ shift: true });
    expect(screen.getByRole("button", { name: "Next" })).toHaveFocus();
    expect(
      window.localStorage.getItem(proposalDetailTourStorageKey(userId)),
    ).toBeNull();

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(
      screen.getByRole("dialog", { name: "Scope and validation" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(
      screen.getByRole("dialog", { name: "Automatic refinement" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(
      screen.getByRole("dialog", { name: "The readiness gate" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(
      screen.getByRole("dialog", { name: "Approve, then graduate" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Finish" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(
      window.localStorage.getItem(proposalDetailTourStorageKey(userId)),
    ).toBe("seen");
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Tour this proposal page" }),
      ).toHaveFocus(),
    );

    unmount();
    render(
      <>
        <TourTargets />
        <ProposalDetailTour userId={userId} />
      </>,
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Tour this proposal page" }),
    );
    expect(
      screen.getByRole("dialog", { name: "The proposal brief" }),
    ).toBeInTheDocument();
  });

  it("supports keyboard dismissal and records the tour as seen", async () => {
    const user = userEvent.setup();
    render(
      <>
        <TourTargets />
        <ProposalDetailTour userId={userId} forceOpen />
      </>,
    );

    await screen.findByRole("dialog");
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(
      window.localStorage.getItem(proposalDetailTourStorageKey(userId)),
    ).toBe("seen");
  });
});
