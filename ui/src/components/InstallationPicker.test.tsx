import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  render,
  screen,
  userEvent,
  waitFor,
} from "@/test/test-utils";
import { InstallationPicker } from "@/components/InstallationPicker";
import {
  fetchInstallations,
  selectInstallation,
  type InstallationSummary,
} from "@/api/auth";

vi.mock("@/api/auth", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/auth")>();
  return {
    ...actual,
    fetchInstallations: vi.fn(),
    selectInstallation: vi.fn(),
  };
});

const SAMPLE: InstallationSummary[] = [
  {
    installationId: 11,
    accountLogin: "acme-inc",
    accountId: 100,
    accountType: "Organization",
    repositorySelection: "all",
    htmlUrl: "https://github.com/organizations/acme-inc/settings/installations/11",
  },
  {
    installationId: 22,
    accountLogin: "alice",
    accountId: 200,
    accountType: "User",
    repositorySelection: "selected",
    htmlUrl: "https://github.com/settings/installations/22",
  },
];

describe("InstallationPicker", () => {
  beforeEach(() => {
    vi.mocked(fetchInstallations).mockReset();
    vi.mocked(selectInstallation).mockReset();
  });

  it("renders the empty-list CTA when GitHub returns no installations", async () => {
    vi.mocked(fetchInstallations).mockResolvedValue([]);
    render(<InstallationPicker />);

    await waitFor(() => {
      expect(screen.getByText("No installations yet")).toBeInTheDocument();
    });
    expect(
      screen.getByRole("link", { name: /Install the Djinn App on a GitHub org/i }),
    ).toBeInTheDocument();
  });

  it("renders a row per installation with account type + scope hint", async () => {
    vi.mocked(fetchInstallations).mockResolvedValue(SAMPLE);
    render(<InstallationPicker />);

    await waitFor(() => {
      expect(screen.getByText("acme-inc")).toBeInTheDocument();
    });
    expect(screen.getByText(/Organization · all current and future repos/)).toBeInTheDocument();
    expect(screen.getByText("alice")).toBeInTheDocument();
    expect(screen.getByText(/User · limited to selected repos/)).toBeInTheDocument();
  });

  it("POSTs the chosen id when a row is clicked", async () => {
    vi.mocked(fetchInstallations).mockResolvedValue(SAMPLE);
    vi.mocked(selectInstallation).mockResolvedValue({
      installationId: 11,
      accountLogin: "acme-inc",
    });
    render(<InstallationPicker />);

    await waitFor(() => {
      expect(screen.getByText("acme-inc")).toBeInTheDocument();
    });
    const row = screen.getByRole("button", { name: /acme-inc/i });
    await userEvent.click(row);

    await waitFor(() => {
      expect(vi.mocked(selectInstallation)).toHaveBeenCalledWith(11);
    });
  });

  it("surfaces a retry button when the list fetch fails", async () => {
    vi.mocked(fetchInstallations).mockRejectedValue(new Error("kaboom"));
    render(<InstallationPicker />);

    await waitFor(() => {
      expect(screen.getByText("Could not load installations")).toBeInTheDocument();
    });
    expect(screen.getByText("kaboom")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Try again/i })).toBeInTheDocument();
  });

  it("surfaces the bind-mutation error inline", async () => {
    vi.mocked(fetchInstallations).mockResolvedValue(SAMPLE);
    vi.mocked(selectInstallation).mockRejectedValue(new Error("nope"));
    render(<InstallationPicker />);

    await waitFor(() => {
      expect(screen.getByText("acme-inc")).toBeInTheDocument();
    });
    await userEvent.click(screen.getByRole("button", { name: /acme-inc/i }));

    await waitFor(() => {
      expect(screen.getByText("nope")).toBeInTheDocument();
    });
  });
});
