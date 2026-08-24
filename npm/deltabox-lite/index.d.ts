/// <reference types="node" />

export interface NativeEnvironmentEntry {
  key: string
  value: string
}

export interface NativeDeltaboxOptions {
  stateDir?: string
  runtimeDir?: string
  fsMode?: 'cbcow-through' | 'git'
  isolation?: 'deltabox' | 'bubblewrap' | 'docker' | 'none'
}

export interface NativeSessionInfo {
  session_id: string
  project: string
  resolved_fs_mode: 'cbcow-through' | 'git'
  durability: string
  isolation: 'deltabox' | 'bubblewrap' | 'docker' | 'none'
  isolation_provider?: 'sandlock' | 'seatbelt' | 'bubblewrap' | 'docker'
  state_provider?: string
  rollback_guarantee: string
  mountpoint?: string
}

export interface NativeExecResult {
  exit_code?: number
  stdout: string
  stderr: string
  duration_ms: number
  truncated: boolean
}

export interface NativeDeltaboxInstance {
  startSession(policy?: string, durability?: 'fast' | 'strict', groupCommitMs?: number): Promise<NativeSessionInfo>
  stopSession(sessionId: string): Promise<NativeSessionInfo>
  exec(
    sessionId: string,
    argv: string[],
    cwd?: string,
    environment?: NativeEnvironmentEntry[],
    label?: string,
  ): Promise<NativeExecResult>
  readFile(sessionId: string, path: string): Promise<Buffer>
  writeFile(sessionId: string, path: string, content: Buffer): Promise<void>
  beginReview(sessionId: string, kind: 'task' | 'tool' | 'risky', label?: string): Promise<unknown>
  reviewDiff(sessionId: string): Promise<unknown>
  reviewCommit(sessionId: string): Promise<unknown>
  reviewAbort(sessionId: string): Promise<unknown>
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
