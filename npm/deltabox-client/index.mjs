import { spawn } from "node:child_process";
import { realpathSync } from "node:fs";
import { isAbsolute, join } from "node:path";

/**
 * A client for one deltabox-lite sandbox session.
 *
 * It spawns `deltabox serve` as a child process and talks to it over
 * stdin/stdout using line-delimited JSON-RPC. One client owns one child
 * process and one session.
 */
export class DeltaboxClient {
  #child;
  #buf = "";
  #pending = new Map();
  #nextId = 1;
  #project;
  #dead = null;

  /** @type {string} the session id, set by DeltaboxClient.start() */
  sessionId = "";
  /** @type {object} the full SessionInfo returned by session.start */
  info = {};

  constructor(child, project) {
    this.#child = child;
    this.#project = project;

    child.stdout.on("data", (chunk) => this.#onData(chunk));
    child.on("error", (err) => this.#fail(err));
    child.on("exit", (code) => this.#fail(new Error(`deltabox serve exited (code ${code})`)));
  }

  #fail(err) {
    this.#dead = err;
    for (const { reject } of this.#pending.values()) reject(err);
    this.#pending.clear();
  }

  /**
   * Start `deltabox serve` in `project` and open a session there.
   * @param {string} project absolute path to the project directory
   * @param {object} [opts]
   * @param {string} [opts.binary] path to the deltabox binary (default: "deltabox" from PATH)
   * @param {string} [opts.fsMode] "cbcow-through" | "cbcow-private" | "copy-private"
   * @param {string} [opts.durability] "fast" | "strict"
   * @returns {Promise<DeltaboxClient>}
   */
  static async start(project, opts = {}) {
    if (!isAbsolute(project)) throw new Error(`project must be an absolute path: ${project}`);
    // The server canonicalizes the project path (resolving symlinks such as
    // macOS /var -> /private/var); resolve it here too so client-side path
    // joins land inside the server's view of the project.
    project = realpathSync(project);
    // serve treats its cwd as the project (the request's `project` field is
    // currently ignored by the server), so the spawn cwd is what matters.
    const child = spawn(opts.binary ?? "deltabox", ["serve"], {
      cwd: project,
      stdio: ["pipe", "pipe", "inherit"],
    });
    const client = new DeltaboxClient(child, project);
    const params = { project };
    if (opts.fsMode) params.fs_mode = opts.fsMode;
    if (opts.durability) params.durability = opts.durability;
    client.info = await client.call("session.start", params);
    client.sessionId = client.info.session_id;
    return client;
  }

  #onData(chunk) {
    this.#buf += chunk.toString();
    let newline;
    while ((newline = this.#buf.indexOf("\n")) >= 0) {
      const line = this.#buf.slice(0, newline).trim();
      this.#buf = this.#buf.slice(newline + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      const waiter = this.#pending.get(msg.id);
      if (!waiter) continue;
      this.#pending.delete(msg.id);
      if (msg.error) waiter.reject(new Error(msg.error.message));
      else waiter.resolve(msg.result);
    }
  }

  /**
   * Send one raw JSON-RPC request and await its result.
   * Prefer the typed helpers below; this is the escape hatch.
   */
  call(method, params = {}) {
    if (this.#dead) return Promise.reject(this.#dead);
    return new Promise((resolve, reject) => {
      const id = this.#nextId++;
      this.#pending.set(id, { resolve, reject });
      this.#child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    });
  }

  #inProject(path) {
    return isAbsolute(path) ? path : join(this.#project, path);
  }

  /** Current SessionInfo (pending change count, mode, taint state, ...). */
  status() {
    return this.call("session.status", { session_id: this.sessionId });
  }

  /**
   * Run a command inside the sandbox.
   * @param {string[]} argv e.g. ["npm", "install"]
   * @param {object} [opts] {cwd, env, label}
   * @returns {Promise<{exit_code, stdout, stderr, duration_ms, truncated}>}
   */
  exec(argv, opts = {}) {
    return this.call("session.exec", { session_id: this.sessionId, argv, ...opts });
  }

  /**
   * Read a file through the sandbox view.
   * @param {string} path absolute, or relative to the project root
   * @returns {Promise<Buffer>}
   */
  async read(path) {
    const res = await this.call("fs.read", { session_id: this.sessionId, path: this.#inProject(path) });
    return Buffer.from(res.content_base64, "base64");
  }

  /**
   * Write a file through the sandbox view.
   * @param {string} path absolute, or relative to the project root
   * @param {string|Buffer} content
   */
  write(path, content) {
    return this.call("fs.write", {
      session_id: this.sessionId,
      path: this.#inProject(path),
      content_base64: Buffer.from(content).toString("base64"),
    });
  }

  /** Freeze the current state as a named savepoint. */
  checkpoint(label) {
    return this.call("checkpoint.create", { session_id: this.sessionId, label });
  }

  /** List pending changes against the accepted state. */
  diff(opts = {}) {
    return this.call("diff.list", { session_id: this.sessionId, ...opts });
  }

  /** Accept the pending changes. */
  commit(opts = {}) {
    return this.call("changes.commit", { session_id: this.sessionId, ...opts });
  }

  /** Discard the pending changes and restore the accepted state. */
  abort(opts = {}) {
    return this.call("changes.abort", { session_id: this.sessionId, ...opts });
  }

  /** Begin a review scope ("task" or "tool"); see docs/session-semantics.md. */
  reviewBegin(kind, label) {
    return this.call("review.begin", { session_id: this.sessionId, kind, label });
  }

  reviewStatus() {
    return this.call("review.status", { session_id: this.sessionId });
  }

  reviewDiff() {
    return this.call("review.diff", { session_id: this.sessionId });
  }

  reviewCommit() {
    return this.call("review.commit", { session_id: this.sessionId });
  }

  reviewAbort() {
    return this.call("review.abort", { session_id: this.sessionId });
  }

  reviewSkip() {
    return this.call("review.skip", { session_id: this.sessionId });
  }

  /** Stop the session, then shut the child process down. */
  async stop() {
    try {
      await this.call("session.stop", { session_id: this.sessionId });
    } finally {
      await this.close();
    }
  }

  /** Shut the child process down without stopping the session. */
  async close() {
    if (this.#child.pid === undefined) return;
    if (this.#child.exitCode !== null) return;
    this.#child.stdin.end();
    const exited = new Promise((resolve) => this.#child.once("exit", resolve));
    const timer = setTimeout(() => this.#child.kill(), 2000);
    await exited;
    clearTimeout(timer);
  }
}
