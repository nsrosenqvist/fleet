/**
 * git-bug v0.10.1 `--format json` payloads.
 *
 * Note: git-bug uses snake_case field names in JSON output (`human_id`,
 * `create_time`, etc.) — different from the camelCase names used in the
 * `--field` selector. The two interfaces are inconsistent in git-bug itself.
 */

export interface GitBugTimestampJson {
  timestamp: number;
  time: string;
}

export interface GitBugUserJson {
  id: string;
  human_id: string;
  name: string;
  login: string;
}

export interface GitBugCommentJson {
  id: string;
  human_id: string;
  author: GitBugUserJson;
  message: string;
  files?: string[];
}

export type GitBugStatus = "open" | "closed";

/** Output of `git-bug bug --format json` (one entry per bug, summary). */
export interface GitBugSummaryJson {
  id: string;
  human_id: string;
  status: GitBugStatus;
  title: string;
  labels: string[];
  author: GitBugUserJson;
  create_time?: GitBugTimestampJson;
  edit_time?: GitBugTimestampJson;
}

/** Output of `git-bug bug show <id> --format json` (full detail). */
export interface GitBugBugJson extends GitBugSummaryJson {
  comments?: GitBugCommentJson[];
  participants?: GitBugUserJson[];
  actors?: GitBugUserJson[];
}
