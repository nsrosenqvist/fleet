import type {
  CreateIssueInput,
  Issue,
  IssueFilters,
  IssueUpdate,
  PluginManifest,
  PluginModule,
  Tracker,
} from "@aoagents/ao-core";
import { runGitBug } from "./git-bug-cli.js";
import type { GitBugBugJson, GitBugSummaryJson } from "./git-bug-types.js";

const ISSUE_CACHE_TTL_MS = 5 * 60_000;
const ISSUE_CACHE_MAX = 500;

function mapIssueState(status: string): Issue["state"] {
  return status === "closed" ? "closed" : "open";
}

function summaryToIssue(s: GitBugSummaryJson): Issue {
  const issue: Issue = {
    id: s.human_id,
    title: s.title,
    description: "",
    url: `git-bug://bug/${s.human_id}`,
    state: mapIssueState(s.status),
    labels: s.labels,
  };
  // git-bug's `login` is "" for users without one; only fall back to `name`.
  if (s.author?.login !== undefined && s.author.login.length > 0) {
    issue.assignee = s.author.login;
  } else if (s.author?.name !== undefined && s.author.name.length > 0) {
    issue.assignee = s.author.name;
  }
  return issue;
}

function bugToIssue(b: GitBugBugJson): Issue {
  const issue = summaryToIssue(b);
  const firstComment = b.comments?.[0]?.message;
  if (firstComment !== undefined && firstComment.length > 0) {
    issue.description = firstComment;
  }
  return issue;
}

function createGitBugTracker(): Tracker {
  const issueCache = new Map<string, { issue: Issue; expiresAt: number }>();
  const inflight = new Map<string, Promise<Issue>>();

  function readCachedIssue(id: string): Issue | null {
    const entry = issueCache.get(id);
    if (!entry) return null;
    if (Date.now() > entry.expiresAt) {
      issueCache.delete(id);
      return null;
    }
    return entry.issue;
  }

  function writeCachedIssue(id: string, issue: Issue): void {
    if (issueCache.size >= ISSUE_CACHE_MAX) {
      const oldest = issueCache.keys().next().value;
      if (oldest !== undefined) issueCache.delete(oldest);
    }
    issueCache.set(id, { issue, expiresAt: Date.now() + ISSUE_CACHE_TTL_MS });
  }

  function invalidate(id: string): void {
    issueCache.delete(id);
  }

  const tracker: Tracker = {
    name: "git-bug",

    async getIssue(identifier, project): Promise<Issue> {
      const cached = readCachedIssue(identifier);
      if (cached) return cached;

      const pending = inflight.get(identifier);
      if (pending) return pending;

      const promise = (async () => {
        const raw = await runGitBug(["bug", "show", identifier, "--format", "json"], {
          cwd: project.path,
        });
        const data = JSON.parse(raw) as GitBugBugJson;
        const issue = bugToIssue(data);
        writeCachedIssue(identifier, issue);
        return issue;
      })();
      inflight.set(identifier, promise);
      try {
        return await promise;
      } finally {
        inflight.delete(identifier);
      }
    },

    async isCompleted(identifier, project): Promise<boolean> {
      const issue = await tracker.getIssue(identifier, project);
      return issue.state === "closed" || issue.state === "cancelled";
    },

    issueUrl(identifier, _project): string {
      return `git-bug://bug/${identifier.replace(/^#/, "")}`;
    },

    issueLabel(url, _project): string {
      const match = url.match(/bug\/([^/]+)/);
      if (match?.[1] !== undefined) return `#${match[1]}`;
      const parts = url.split("/");
      const last = parts[parts.length - 1];
      return last !== undefined && last.length > 0 ? `#${last}` : url;
    },

    branchName(identifier, _project): string {
      // Branch name AO uses for the worktree. `agent/<bug-id>` makes the
      // origin of the branch unambiguous when an engineer is reviewing
      // remote branches alongside their own work. AO callers that want a
      // slug should call getIssue first and append it.
      const id = identifier.replace(/^#/, "");
      return `agent/${id}`;
    },

    async generatePrompt(identifier, project): Promise<string> {
      const issue = await tracker.getIssue(identifier, project);
      const lines = [
        `You are working on git-bug issue ${issue.id}: ${issue.title}`,
        `Issue handle: ${issue.url}`,
        "",
      ];
      if (issue.labels.length > 0) {
        lines.push(`Labels: ${issue.labels.join(", ")}`);
      }
      if (issue.description.length > 0) {
        lines.push("## Description", "", issue.description);
      }
      lines.push(
        "",
        "The issue context above was fetched via `git-bug bug show`. You can re-fetch with `git-bug bug show " +
          issue.id +
          "` if you need to inspect raw fields, but the orchestrator-supplied context is current.",
        "",
        "Implement the change described above. Commit using Conventional Commits and open a PR per AGENTS.md.",
      );
      return lines.join("\n");
    },

    async listIssues(filters: IssueFilters, project): Promise<Issue[]> {
      // NB: `git-bug bug ls` parses "ls" as a query string and matches zero
      // bugs. The canonical "list all" invocation in v0.10.1 is `git-bug bug`
      // (with optional query/flags). Do not add "ls" back.
      const args = ["bug", "--format", "json"];
      if (filters.state === "closed") {
        args.push("--status", "closed");
      } else if (filters.state === "all") {
        // omit --status to include both open and closed
      } else {
        args.push("--status", "open");
      }
      if (filters.labels && filters.labels.length > 0) {
        for (const label of filters.labels) {
          args.push("--label", label);
        }
      }
      const raw = await runGitBug(args, { cwd: project.path });
      const parsed = JSON.parse(raw) as GitBugSummaryJson[];
      const limit = filters.limit ?? parsed.length;
      return parsed.slice(0, limit).map(summaryToIssue);
    },

    async updateIssue(identifier, update: IssueUpdate, project): Promise<void> {
      invalidate(identifier);

      // State transitions. git-bug has no "in_progress" — that's an AO concept.
      if (update.state === "closed") {
        await runGitBug(["bug", "status", "close", identifier], { cwd: project.path });
      } else if (update.state === "open") {
        await runGitBug(["bug", "status", "open", identifier], { cwd: project.path });
      }

      if (update.removeLabels && update.removeLabels.length > 0) {
        await runGitBug(["bug", "label", "rm", identifier, ...update.removeLabels], {
          cwd: project.path,
        });
      }
      if (update.labels && update.labels.length > 0) {
        await runGitBug(["bug", "label", "new", identifier, ...update.labels], {
          cwd: project.path,
        });
      }

      // git-bug has no assignee concept; treat as no-op rather than error so AO
      // can pass IssueUpdate.assignee through generically.

      if (update.comment !== undefined && update.comment.length > 0) {
        await runGitBug(["bug", "comment", "new", identifier, "-m", update.comment], {
          cwd: project.path,
        });
      }
    },

    async createIssue(input: CreateIssueInput, project): Promise<Issue> {
      const args = ["bug", "new", "--non-interactive", "-t", input.title];
      if (input.description.length > 0) {
        args.push("-m", input.description);
      }
      const raw = await runGitBug(args, { cwd: project.path });
      // git-bug bug new prints "<id> <title>\n"; first whitespace-delimited token is the human ID.
      const id = raw.trim().split(/\s+/)[0];
      if (id === undefined || id.length === 0) {
        throw new Error(`git-bug bug new returned no id (raw: ${JSON.stringify(raw)})`);
      }
      if (input.labels && input.labels.length > 0) {
        await runGitBug(["bug", "label", "new", id, ...input.labels], { cwd: project.path });
      }
      return tracker.getIssue(id, project);
    },

    async preflight(context): Promise<void> {
      // 1. git-bug binary present?
      try {
        await runGitBug(["version"], { cwd: context.project.path, timeoutMs: 5_000 });
      } catch (err) {
        throw new Error(
          "git-bug CLI not found or unreadable. Install: https://github.com/git-bug/git-bug — see ~/.lima/fleet-vm.yaml for the in-VM source build.",
          { cause: err },
        );
      }
      // 2. project.path is a git repo? (git-bug subcommands all fail without one.)
      try {
        await runGitBug(["user", "ls"], { cwd: context.project.path, timeoutMs: 5_000 });
      } catch (err) {
        throw new Error(
          `Project path ${context.project.path} is not a git repo recognized by git-bug. Run \`git init\` and \`git-bug user new\` there.`,
          { cause: err },
        );
      }
    },
  };

  return tracker;
}

export const manifest: PluginManifest = {
  name: "git-bug",
  slot: "tracker",
  description: "Tracker plugin: git-bug (distributed bug tracker embedded in git refs)",
  version: "0.1.0",
};

export function create(): Tracker {
  return createGitBugTracker();
}

const pluginModule: PluginModule<Tracker> = { manifest, create };
export default pluginModule;
