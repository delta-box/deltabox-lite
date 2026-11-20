"use strict";
// ChildProcess-style wrapper over the native `spawn()` handle, and an
// options-object `exec()`.
//
// On Unix the native layer hands over the read ends of the command's stdout
// and stderr pipes as file descriptors, and they become ordinary Node
// `Readable` streams with real backpressure:
//   - Node: `net.Socket({ fd })`, which reads on the event loop.
//   - Bun:  `Bun.file(fd).stream()`. Bun's `net.Socket` drops pipe
//     descriptors, and `fs.createReadStream` would block thread-pool threads
//     (in Node too), so neither is used. Bun does not close the descriptor
//     itself; this wrapper closes it exactly once.
// On Windows there is no descriptor to hand over, so each stream pulls chunks
// from the native handle (`readStdout`/`readStderr`), one read at a time.

const { EventEmitter } = require("node:events");
const fs = require("node:fs");
const net = require("node:net");
const { Readable } = require("node:stream");
const { randomUUID } = require("node:crypto");

const isBun = typeof globalThis.Bun !== "undefined";

/** Wrap a pipe read end the caller now owns. */
function fdStream(fd) {
  if (!isBun) return new net.Socket({ fd, readable: true, writable: false });
  const stream = Readable.fromWeb(globalThis.Bun.file(fd).stream());
  // `close` follows normal EOF, an error and destroy() alike, once per
  // stream; the guard keeps the descriptor closed exactly once regardless.
  let open = true;
  const closeFd = () => {
    if (!open) return;
    open = false;
    try {
      fs.closeSync(fd);
    } catch {}
  };
  stream.once("close", closeFd);
  return stream;
}

/** A stream that pulls chunks from the native handle (Windows). */
function pullStream(read) {
  return new Readable({
    read() {
      read().then(
        (chunk) => this.push(chunk ?? null),
        (error) => this.destroy(error),
      );
    },
  });
}

function outputStream(native, which) {
  const fd = which === "stdout" ? native.takeStdoutFd() : native.takeStderrFd();
  if (fd !== null && fd !== undefined) return fdStream(fd);
  return pullStream(() => (which === "stdout" ? native.readStdout() : native.readStderr()));
}

function environment(env) {
  return env ? Object.entries(env).map(([key, value]) => ({ key, value: String(value) })) : undefined;
}

function nativeOptions(options) {
  return {
    argv: options.argv,
    cwd: options.cwd,
    environment: environment(options.env),
    label: options.label,
    executionProfile: options.executionProfile,
    networkAllow: options.networkAllow,
    fsReadAllow: options.fsReadAllow,
    fsWriteAllow: options.fsWriteAllow,
  };
}

/**
 * A command running in a Deltabox session. Events: `exit` (code, signal)
 * once the command has finished, `close` (code, signal) once it has finished
 * and both output streams have closed, and `error` if it failed to run.
 */
class DeltaboxChildProcess extends EventEmitter {
  constructor(native) {
    super();
    this._native = native;
    this.stdout = outputStream(native, "stdout");
    this.stderr = outputStream(native, "stderr");
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this._done = native.wait();
    // Handled here so a failure never surfaces as an unhandled rejection;
    // `result` and the `error` event still report it.
    this._done.then(
      (result) => {
        this.exitCode = result.exit_code;
        this.emit("exit", result.exit_code, null);
      },
      (error) => {
        if (this.listenerCount("error") > 0) this.emit("error", error);
      },
    );
    const closed = (stream) =>
      new Promise((resolve) => (stream.closed ? resolve() : stream.once("close", resolve)));
    Promise.allSettled([this._done, closed(this.stdout), closed(this.stderr)]).then(([done]) => {
      if (done.status === "fulfilled") this.emit("close", done.value.exit_code, null);
    });
  }

  /** The command's process id, or undefined until it has started. */
  get pid() {
    return this._native.pid ?? undefined;
  }

  /**
   * The command's result once it has finished. Awaiting it starts draining
   * any output stream nobody is reading, so a command that writes more than
   * a pipe holds cannot block on it. Its stdout/stderr fields are empty: the
   * output went to the streams.
   */
  get result() {
    for (const stream of [this.stdout, this.stderr]) {
      if (stream.readableFlowing === null) stream.resume();
    }
    return this._done;
  }

  /** End the command's whole process tree. */
  kill() {
    this.killed = true;
    this._native.kill();
    return true;
  }
}

/**
 * Start a command in a session and return a `ChildProcess`-style handle with
 * `stdout`/`stderr` streams. Read both streams, or await `result`, so the
 * command never blocks on a full pipe.
 */
function spawn(deltabox, sessionId, options) {
  return new DeltaboxChildProcess(deltabox.spawn(sessionId, nativeOptions(options)));
}

/**
 * Run a command to completion and return its buffered result. `signal`
 * cancels it: its whole process tree is killed.
 */
async function exec(deltabox, sessionId, options) {
  const execId = options.signal ? randomUUID() : undefined;
  const cancel = () => deltabox.cancelExec(execId);
  if (options.signal?.aborted) throw options.signal.reason ?? new Error("command cancelled");
  options.signal?.addEventListener("abort", cancel, { once: true });
  try {
    const native = nativeOptions(options);
    return await deltabox.exec(
      sessionId,
      native.argv,
      native.cwd,
      native.environment,
      native.label,
      native.executionProfile,
      native.networkAllow,
      execId,
      native.fsReadAllow,
      native.fsWriteAllow,
    );
  } finally {
    options.signal?.removeEventListener("abort", cancel);
  }
}

module.exports = { spawn, exec, DeltaboxChildProcess };
