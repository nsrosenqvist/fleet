import { afterEach, describe, expect, it, vi } from "vitest";
import type { ProjectConfig } from "@aoagents/ao-core";
import { create, manifest } from "./index.js";

vi.mock("./git-bug-cli.js", () => ({
  runGitBug: vi.fn(),
}));

const { runGitBug } = await import("./git-bug-cli.js");
const runGitBugMock = vi.mocked(runGitBug);

const project: ProjectConfig = {
  name: "sandbox",
  path: "/tmp/git-bug-sandbox",
  defaultBranch: "main",
  sessionPrefix: "ao",
};

afterEach(() => {
  runGitBugMock.mockReset();
});

describe("manifest", () => {
  it("declares the tracker slot and name git-bug", () => {
    expect(manifest.name).toBe("git-bug");
    expect(manifest.slot).toBe("tracker");
  });
});

describe("getIssue", () => {
  it("maps a git-bug bug show JSON payload (snake_case) to an AO Issue", async () => {
    runGitBugMock.mockResolvedValueOnce(
      JSON.stringify({
        id: "33ba5f6f...",
        human_id: "33ba5f6",
        title: "Fix typo in README",
        status: "open",
        labels: ["agent-eligible", "docs"],
        author: { id: "u1", human_id: "u1", name: "Niklas", login: "niklas" },
        comments: [
          {
            id: "c1",
            human_id: "c1",
            author: { id: "u1", human_id: "u1", name: "Niklas", login: "niklas" },
            message: "There is a typo on line 12.",
          },
        ],
      }),
    );

    const tracker = create();
    const issue = await tracker.getIssue("33ba5f6", project);

    expect(issue).toEqual({
      id: "33ba5f6",
      title: "Fix typo in README",
      description: "There is a typo on line 12.",
      url: "git-bug://bug/33ba5f6",
      state: "open",
      labels: ["agent-eligible", "docs"],
      assignee: "niklas",
    });
    expect(runGitBugMock).toHaveBeenCalledExactlyOnceWith(
      ["bug", "show", "33ba5f6", "--format", "json"],
      { cwd: "/tmp/git-bug-sandbox" },
    );
  });

  it("falls back to author.name when login is empty (the git-bug default)", async () => {
    runGitBugMock.mockResolvedValueOnce(
      JSON.stringify({
        id: "x",
        human_id: "abc",
        title: "T",
        status: "open",
        labels: [],
        author: { id: "u", human_id: "u", name: "Niklas", login: "" },
      }),
    );
    const tracker = create();
    const issue = await tracker.getIssue("abc", project);
    expect(issue.assignee).toBe("Niklas");
  });

  it("caches a fetched issue and deduplicates concurrent reads", async () => {
    runGitBugMock.mockResolvedValue(
      JSON.stringify({
        id: "x",
        human_id: "abc",
        title: "T",
        status: "open",
        labels: [],
        author: { id: "u", human_id: "u", name: "N", login: "" },
      }),
    );

    const tracker = create();
    const [a, b] = await Promise.all([
      tracker.getIssue("abc", project),
      tracker.getIssue("abc", project),
    ]);
    expect(a).toBe(b);
    expect(runGitBugMock).toHaveBeenCalledTimes(1);

    await tracker.getIssue("abc", project);
    expect(runGitBugMock).toHaveBeenCalledTimes(1);
  });
});

describe("isCompleted", () => {
  it("returns true for a closed bug", async () => {
    runGitBugMock.mockResolvedValueOnce(
      JSON.stringify({
        id: "x",
        human_id: "abc",
        title: "T",
        status: "closed",
        labels: [],
        author: { id: "u", human_id: "u", name: "N", login: "" },
      }),
    );
    const tracker = create();
    expect(await tracker.isCompleted("abc", project)).toBe(true);
  });

  it("returns false for an open bug", async () => {
    runGitBugMock.mockResolvedValueOnce(
      JSON.stringify({
        id: "x",
        human_id: "abc",
        title: "T",
        status: "open",
        labels: [],
        author: { id: "u", human_id: "u", name: "N", login: "" },
      }),
    );
    const tracker = create();
    expect(await tracker.isCompleted("abc", project)).toBe(false);
  });
});

describe("branchName / issueUrl / issueLabel", () => {
  const tracker = create();
  it("strips a leading hash from branch name", () => {
    expect(tracker.branchName("#abc123", project)).toBe("agent/abc123");
  });
  it("builds an issue handle URL", () => {
    expect(tracker.issueUrl("abc123", project)).toBe("git-bug://bug/abc123");
  });
  it("extracts a label from an issue URL", () => {
    expect(tracker.issueLabel?.("git-bug://bug/abc123", project)).toBe("#abc123");
  });
});

describe("listIssues", () => {
  it("uses `git-bug bug` (no `ls`) and passes labels through one flag per label", async () => {
    runGitBugMock.mockResolvedValueOnce(
      JSON.stringify([
        {
          id: "x",
          human_id: "abc",
          title: "T",
          status: "open",
          labels: ["agent-eligible"],
          author: { id: "u", human_id: "u", name: "N", login: "" },
        },
      ]),
    );
    const tracker = create();
    const issues = await tracker.listIssues?.(
      { state: "open", labels: ["agent-eligible"] },
      project,
    );
    expect(issues).toHaveLength(1);
    expect(runGitBugMock).toHaveBeenCalledWith(
      ["bug", "--format", "json", "--status", "open", "--label", "agent-eligible"],
      { cwd: project.path },
    );
  });

  it("omits --status when filter.state is 'all'", async () => {
    runGitBugMock.mockResolvedValueOnce("[]");
    const tracker = create();
    await tracker.listIssues?.({ state: "all" }, project);
    expect(runGitBugMock).toHaveBeenCalledWith(["bug", "--format", "json"], {
      cwd: project.path,
    });
  });
});

describe("updateIssue", () => {
  it("issues label-new for added labels and label-rm for removed labels", async () => {
    runGitBugMock.mockResolvedValue("");
    const tracker = create();
    await tracker.updateIssue?.(
      "abc",
      { labels: ["agent-working"], removeLabels: ["agent-eligible"] },
      project,
    );
    expect(runGitBugMock).toHaveBeenCalledWith(["bug", "label", "rm", "abc", "agent-eligible"], {
      cwd: project.path,
    });
    expect(runGitBugMock).toHaveBeenCalledWith(["bug", "label", "new", "abc", "agent-working"], {
      cwd: project.path,
    });
  });

  it("closes via `bug status close` and posts comments via `bug comment new`", async () => {
    runGitBugMock.mockResolvedValue("");
    const tracker = create();
    await tracker.updateIssue?.("abc", { state: "closed", comment: "done" }, project);
    expect(runGitBugMock).toHaveBeenCalledWith(["bug", "status", "close", "abc"], {
      cwd: project.path,
    });
    expect(runGitBugMock).toHaveBeenCalledWith(["bug", "comment", "new", "abc", "-m", "done"], {
      cwd: project.path,
    });
  });
});
