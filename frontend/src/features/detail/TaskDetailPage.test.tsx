import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { beforeEach, describe, expect, it, vi } from "vitest";
import TaskDetailPage from "./TaskDetailPage";
import type { Project } from "../../types/project";
import type { Run } from "../../types/run";
import type { Task } from "../../types/task";

vi.mock("../../hooks/useRunList", () => ({
  useRunList: vi.fn(),
}));

vi.mock("../../hooks/useCommentList", () => ({
  useCommentList: vi.fn(),
}));

vi.mock("../../hooks/useResumeSession", () => ({
  useResumeSession: vi.fn(() => ({
    mutate: vi.fn(),
    isPending: false,
  })),
}));

vi.mock("../../components/Toast", () => ({
  useToast: () => ({ showToast: vi.fn() }),
}));

import { useRunList } from "../../hooks/useRunList";
import { useCommentList } from "../../hooks/useCommentList";

function mockViewport(matches: boolean) {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
}

const project: Project = {
  id: "proj-1",
  name: "OmniAgent",
  key: "OMNI",
  workspacePath: "/tmp/omni",
  createdAt: "2026-01-01T00:00:00Z",
  updatedAt: "2026-01-01T00:00:00Z",
};

const task: Task = {
  id: "OMNI-001",
  projectId: "proj-1",
  seq: 1,
  title: "Implement transcript detail",
  description: "Show the original request and the agent result.",
  acceptanceCriteria: "Terminal transcript is visible.",
  agent: "codex",
  role: "coder",
  status: "completed",
  createdAt: "2026-01-01T00:00:00Z",
  updatedAt: "2026-01-01T00:00:00Z",
};

const run: Run = {
  id: "run-1",
  runNumber: 1,
  input: "implement detail screen",
  exitCode: 0,
  logPath: "/tmp/run.log",
  logTail: [
    '{"type":"event_msg","timestamp":"2026-05-27T10:00:00Z","payload":{"type":"agent_message","message":"Implemented terminal transcript.","phase":"final"}}',
    "npm --prefix frontend run build",
  ].join("\n"),
  startedAt: "2026-05-27T09:58:00Z",
  endedAt: "2026-05-27T10:01:00Z",
};

function mockQueries(runs: Run[] = [run]) {
  vi.mocked(useRunList).mockReturnValue({
    data: runs,
    isLoading: false,
    isError: false,
    isPending: false,
    refetch: vi.fn(),
  } as unknown as ReturnType<typeof useRunList>);
  vi.mocked(useCommentList).mockReturnValue({
    data: [],
    isLoading: false,
    isError: false,
    isPending: false,
    refetch: vi.fn(),
  } as unknown as ReturnType<typeof useCommentList>);
}

describe("TaskDetailPage", () => {
  beforeEach(() => {
    mockViewport(true);
  });

  it("renders conversation snapshot beside a terminal transcript", () => {
    mockQueries();

    render(
      <MemoryRouter>
        <TaskDetailPage task={task} project={project} />
      </MemoryRouter>,
    );

    expect(screen.getByLabelText("Input and final output")).toHaveTextContent(
      "Show the original request and the agent result.",
    );
    expect(screen.getByLabelText("Input and final output")).toHaveTextContent(
      "Implemented terminal transcript.",
    );
    expect(screen.getByLabelText("Agent terminal transcript")).toHaveTextContent(
      "codex run OMNI-001 --role coder",
    );
    expect(screen.getByLabelText("Agent terminal transcript")).toHaveTextContent(
      "npm --prefix frontend run build",
    );
  });

  it("toggles the transcript panel without removing the main conversation", async () => {
    const user = userEvent.setup();
    mockQueries();

    render(
      <MemoryRouter>
        <TaskDetailPage task={task} project={project} />
      </MemoryRouter>,
    );

    await user.click(screen.getByRole("button", { name: "Hide transcript panel" }));

    expect(screen.queryByLabelText("Agent terminal transcript")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Input and final output")).toHaveTextContent(
      "Show the original request and the agent result.",
    );

    await user.click(screen.getByRole("button", { name: "Show transcript panel" }));

    expect(screen.getByLabelText("Agent terminal transcript")).toHaveTextContent(
      "codex run OMNI-001 --role coder",
    );
  });

  it("defaults the transcript panel closed on narrow viewports", async () => {
    const user = userEvent.setup();
    mockViewport(false);
    mockQueries();

    render(
      <MemoryRouter>
        <TaskDetailPage task={task} project={project} />
      </MemoryRouter>,
    );

    expect(screen.queryByLabelText("Agent terminal transcript")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Input and final output")).toHaveTextContent(
      "Implemented terminal transcript.",
    );

    await user.click(screen.getByRole("button", { name: "Show transcript panel" }));

    expect(screen.getByLabelText("Agent terminal transcript")).toHaveTextContent(
      "npm --prefix frontend run build",
    );
  });
});
