import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { WslUsageDetectedDialog } from "@/components/WslUsageDetectedDialog";

vi.mock("@/components/ui/dialog", () => ({
  Dialog: ({ open, children }: { open: boolean; children: React.ReactNode }) =>
    open ? <div>{children}</div> : null,
  DialogContent: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogHeader: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogTitle: ({ children }: { children: React.ReactNode }) => (
    <h1>{children}</h1>
  ),
  DialogDescription: ({ children }: { children: React.ReactNode }) => (
    <p>{children}</p>
  ),
  DialogFooter: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
}));

const settings = {
  showInTray: true,
  minimizeToTrayOnClose: true,
  wslUsagePromptConfirmed: undefined as boolean | undefined,
  enableWslUsageSync: false,
};

const detectWslSources = vi.fn();
const saveSettings = vi.fn();
const syncSessionUsage = vi.fn();

vi.mock("@/lib/query", () => ({
  useSettingsQuery: () => ({ data: settings }),
}));

vi.mock("@/lib/api", () => ({
  settingsApi: {
    save: (...args: unknown[]) => saveSettings(...args),
  },
  usageApi: {
    detectWslSources: () => detectWslSources(),
    syncSessionUsage: () => syncSessionUsage(),
  },
}));

const Wrapper = ({ children }: { children: React.ReactNode }) => (
  <QueryClientProvider client={new QueryClient()}>
    {children}
  </QueryClientProvider>
);

describe("WslUsageDetectedDialog", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    settings.wslUsagePromptConfirmed = undefined;
    settings.enableWslUsageSync = false;
    saveSettings.mockResolvedValue(undefined);
    syncSessionUsage.mockResolvedValue({});
  });

  it("stays closed when no tools are detected in WSL", async () => {
    detectWslSources.mockResolvedValue([]);

    render(<WslUsageDetectedDialog />, { wrapper: Wrapper });

    await waitFor(() => expect(detectWslSources).toHaveBeenCalled());
    expect(screen.queryByText("wslUsageNotice.title")).not.toBeInTheDocument();
  });

  it("lists detected distros and their tools", async () => {
    detectWslSources.mockResolvedValue([
      { distro: "Ubuntu", tools: ["Claude Code", "Codex"] },
    ]);

    render(<WslUsageDetectedDialog />, { wrapper: Wrapper });

    expect(await screen.findByText("wslUsageNotice.title")).toBeInTheDocument();
    expect(screen.getByText("Ubuntu")).toBeInTheDocument();
    expect(screen.getByText("Claude Code、Codex")).toBeInTheDocument();
  });

  it("enables sync and runs an immediate sync when confirmed", async () => {
    detectWslSources.mockResolvedValue([
      { distro: "Ubuntu", tools: ["Claude Code"] },
    ]);

    render(<WslUsageDetectedDialog />, { wrapper: Wrapper });
    await screen.findByText("wslUsageNotice.title");

    await userEvent.click(
      screen.getByRole("button", { name: "wslUsageNotice.confirm" }),
    );

    await waitFor(() => expect(saveSettings).toHaveBeenCalled());
    expect(saveSettings.mock.calls[0][0]).toMatchObject({
      enableWslUsageSync: true,
      wslUsagePromptConfirmed: true,
    });
    expect(syncSessionUsage).toHaveBeenCalled();
  });

  it("records the prompt as answered without enabling sync when declined", async () => {
    detectWslSources.mockResolvedValue([
      { distro: "Ubuntu", tools: ["Claude Code"] },
    ]);

    render(<WslUsageDetectedDialog />, { wrapper: Wrapper });
    await screen.findByText("wslUsageNotice.title");

    await userEvent.click(
      screen.getByRole("button", { name: "wslUsageNotice.decline" }),
    );

    await waitFor(() => expect(saveSettings).toHaveBeenCalled());
    expect(saveSettings.mock.calls[0][0]).toMatchObject({
      enableWslUsageSync: false,
      wslUsagePromptConfirmed: true,
    });
    // 拒绝时不应触发同步——否则"暂不开启"就名不副实了
    expect(syncSessionUsage).not.toHaveBeenCalled();
  });

  it("does not probe WSL again once the user has answered", async () => {
    settings.wslUsagePromptConfirmed = true;
    detectWslSources.mockResolvedValue([
      { distro: "Ubuntu", tools: ["Claude Code"] },
    ]);

    render(<WslUsageDetectedDialog />, { wrapper: Wrapper });

    await waitFor(() => expect(detectWslSources).not.toHaveBeenCalled());
    expect(screen.queryByText("wslUsageNotice.title")).not.toBeInTheDocument();
  });
});
