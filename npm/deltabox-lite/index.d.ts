/// <reference types="node" />

export interface NativeEnvironmentEntry {
  key: string
  value: string
}

export interface NativeSessionInfo {
  sessionId: string
  project: string
  resolvedFsMode: string
  durability: string
  mountpoint?: string
}

export interface NativeExecResult {
  exitCode?: number
  stdout: string
  stderr: string
  durationMs: number
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
    trusted?: boolean,
    stateDir?: string,
    runtimeDir?: string,
  ): NativeDeltaboxInstance
}

export interface DeltaboxNativeBinding {
  NativeDeltabox: NativeDeltaboxConstructor
}

export function binaryPath(): string
export function nativePath(): string
export function loadNative(): DeltaboxNativeBinding
