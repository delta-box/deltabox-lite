/// <reference types="node" />

export interface NativeEnvironmentEntry {
  key: string
  value: string
}

/**
 * Configuration snapshot for one embedded coordinator, fixed at construction.
 * `fsMode` is the storage mode every session on this coordinator starts in;
 * `isolation` picks how commands run. Neither can be changed later, because the
 * coordinator and its active filesystem state live for the whole process.
 */
export interface NativeDeltaboxOptions {
  stateDir?: string
  runtimeDir?: string
  fsMode?: 'cbcow-through' | 'cbcow-private' | 'copy-private'
  /**
   * `deltabox` keeps the session's own sandbox and deny set. `none` lets
   * commands share the Node process ambient environment, so a host has to name
   * it explicitly; the native transport is the only one that can offer it.
   */
  isolation?: 'deltabox' | 'none'
}

export interface NativeSessionInfo {
  session_id: string
  project: string
  started_at_ms: number
  status: 'running' | 'stopped'
  checkpoint_count: number
  pending_changes: number
  requested_fs_mode: 'cbcow-through' | 'cbcow-private' | 'copy-private'
  resolved_fs_mode: 'cbcow-through' | 'cbcow-private' | 'copy-private'
  host_visibility: 'immediate' | 'on-commit'
  durability: string
  group_commit_ms?: number
  isolation: 'deltabox' | 'none'
  /** Resolved OS mechanism behind `deltabox` isolation, absent under `none`. */
  isolation_provider: string | null
  state_provider: string | null
  fallback_reason: string | null
  rollback_guarantee: string
  tainted: boolean
  mountpoint?: string
}

export interface NativeExecResult {
  exit_code: number | null
  stdout: string
  stderr: string
  duration_ms: number
  truncated: boolean
}

export interface NativeCheckpointInfo {
  id: string
  label: string | null
  created_at_ms: number
  change_count: number
  committed: boolean
}

export interface NativeActiveReview {
  checkpoint_id: string
  kind: 'task' | 'tool' | 'risky'
  label: string | null
  started_at_ms: number
}

export interface NativeChange {
  path: string
  kind: 'added' | 'modified' | 'deleted'
  old_size: number | null
  new_size: number | null
  is_binary: boolean
  added_lines: number
  removed_lines: number
  text_diff?: string
}

export interface NativeDiffResult {
  from: string | null
  to: string | null
  pending_changes: number
  changes: NativeChange[]
}

export interface NativeCommitResult {
  checkpoint_id: string
  applied: number
}

export interface NativeAbortResult {
  checkpoint_id: string
  restored: number
}

export interface NativeDeltaboxInstance {
  startSession(
    policy?: string,
    durability?: 'fast' | 'strict',
    groupCommitMs?: number,
    filesystemMode?: 'auto' | 'cbcow-through' | 'cbcow-private' | 'copy-private',
    requireFilesystemMode?: boolean,
  ): Promise<NativeSessionInfo>
  stopSession(sessionId: string): Promise<NativeSessionInfo>
  sessionStatus(sessionId: string): Promise<NativeSessionInfo>
  exec(
    sessionId: string,
    argv: string[],
    cwd?: string,
    environment?: NativeEnvironmentEntry[],
    label?: string,
  ): Promise<NativeExecResult>
  readFile(sessionId: string, path: string): Promise<Buffer>
  writeFile(sessionId: string, path: string, content: Buffer): Promise<void>
  recordFileChanges(sessionId: string, paths: string[]): Promise<void>
  checkpoint(sessionId: string, label?: string): Promise<NativeCheckpointInfo>
  diff(sessionId: string, from?: string, to?: string): Promise<NativeDiffResult>
  commit(sessionId: string, checkpoint?: string): Promise<NativeCommitResult>
  abort(sessionId: string, checkpoint?: string): Promise<NativeAbortResult>
  beginReview(sessionId: string, kind: 'task' | 'tool' | 'risky', label?: string): Promise<NativeActiveReview>
  reviewStatus(sessionId: string): Promise<NativeActiveReview | null>
  reviewDiff(sessionId: string): Promise<NativeDiffResult>
  reviewCommit(sessionId: string): Promise<NativeCommitResult>
  reviewAbort(sessionId: string): Promise<NativeAbortResult>
  reviewSkip(sessionId: string): Promise<NativeActiveReview>
}

export interface NativeDeltaboxConstructor {
  new(
    projectRoot: string,
    helperBinary: string,
    options?: NativeDeltaboxOptions,
  ): NativeDeltaboxInstance
}

export interface DeltaboxNativeBinding {
  NativeDeltabox: NativeDeltaboxConstructor
}

export function binaryPath(): string
export function nativePath(): string
export function loadNative(): DeltaboxNativeBinding
