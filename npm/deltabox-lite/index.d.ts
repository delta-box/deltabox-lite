/// <reference types="node" />

export interface NativeEnvironmentEntry {
  key: string
  value: string
}

export interface NativeDeltaboxOptions {
  stateDir?: string
  runtimeDir?: string
  fsMode: 'cbcow-through' | 'cbcow-private' | 'copy-private' | 'git'
  isolation: 'deltabox' | 'bubblewrap' | 'docker' | 'none'
}

export interface NativeSessionInfo {
  session_id: string
  project: string
  started_at_ms: number
  status: 'running' | 'stopped'
  checkpoint_count: number
  pending_changes: number
  requested_fs_mode: NativeDeltaboxOptions['fsMode']
  resolved_fs_mode: NativeDeltaboxOptions['fsMode']
  host_visibility: 'immediate' | 'on-commit'
  durability: string
  group_commit_ms?: number
  isolation: NativeDeltaboxOptions['isolation']
  isolation_provider?: 'sandlock' | 'seatbelt' | 'bubblewrap' | 'docker'
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
  startSession(policy?: string, durability?: 'fast' | 'strict', groupCommitMs?: number): Promise<NativeSessionInfo>
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
    options: NativeDeltaboxOptions,
  ): NativeDeltaboxInstance
}

export interface DeltaboxNativeBinding {
  NativeDeltabox: NativeDeltaboxConstructor
}

export function binaryPath(): string
export function nativePath(): string
export function loadNative(): DeltaboxNativeBinding
