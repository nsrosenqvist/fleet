import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export interface RunGitBugOptions {
  cwd: string;
  timeoutMs?: number;
  stdin?: string;
}

export interface GitBugError extends Error {
  stdout?: string;
  stderr?: string;
  exitCode?: number;
}

/**
 * Shell out to the `git-bug` CLI. Never invokes a shell — argv is passed
 * directly to execFile so no escaping is required (and no injection risk).
 */
export async function runGitBug(args: readonly string[], opts: RunGitBugOptions): Promise<string> {
  const timeout = opts.timeoutMs ?? 30_000;
  try {
    const { stdout } = await execFileAsync("git-bug", args, {
      cwd: opts.cwd,
      timeout,
      maxBuffer: 16 * 1024 * 1024,
      windowsHide: true,
    });
    return stdout;
  } catch (raw) {
    const err = raw as NodeJS.ErrnoException & {
      stdout?: string | Buffer;
      stderr?: string | Buffer;
      code?: number | string;
    };
    const message = `git-bug ${args.slice(0, 3).join(" ")} failed: ${err.message}`;
    const wrapped: GitBugError = new Error(message, { cause: err });
    const stdout = typeof err.stdout === "string" ? err.stdout : err.stdout?.toString();
    const stderr = typeof err.stderr === "string" ? err.stderr : err.stderr?.toString();
    if (stdout !== undefined) wrapped.stdout = stdout;
    if (stderr !== undefined) wrapped.stderr = stderr;
    if (typeof err.code === "number") wrapped.exitCode = err.code;
    throw wrapped;
  }
}
