#!/usr/bin/env node

const assert = require('node:assert/strict')
const { mkdtemp, readFile, realpath, rm } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [addonPath, helperBinary, mode = 'cbcow-through'] = process.argv.slice(2)
  if (!addonPath || !helperBinary || !['cbcow-through', 'git'].includes(mode)) {
    throw new Error('usage: smoke.cjs <addon.node> <deltabox> [cbcow-through|git]')
  }
  const { NativeDeltabox } = require(addonPath)
  const project = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-smoke-')))
  const state = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-state-')))
  const runtime = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-runtime-')))
  const file = join(project, 'native-buffer.txt')
  process.env.DELTABOX_NATIVE_SHARED_ENV = 'shared-from-node'
  const gitMode = mode === 'git'
  const native = new NativeDeltabox(project, helperBinary, {
    stateDir: state,
    runtimeDir: runtime,
    fsMode: mode,
    isolation: gitMode ? 'bubblewrap' : 'none',
  })
  let session
  try {
    session = await native.startSession(undefined, 'fast')
    assert.equal(session.resolved_fs_mode, mode)
    assert.equal(session.durability, 'process-crash')
    assert.equal(session.mountpoint === undefined, gitMode)

    const exec = await native.exec(
      session.session_id,
      ['/bin/sh', '-c', 'printf "%s:%s" "$DELTABOX_NATIVE_SHARED_ENV" "$EXPLICIT_VALUE"'],
      project,
      [{ key: 'EXPLICIT_VALUE', value: 'explicit' }],
      'native smoke',
    )
    assert.equal(exec.exit_code, 0)
    assert.equal(exec.stdout, gitMode ? ':explicit' : 'shared-from-node:explicit')

    await native.beginReview(session.session_id, 'task', 'native smoke')
    const content = Buffer.from([0, 1, 2, 253, 254, 255])
    await native.writeFile(session.session_id, file, content)
    assert.deepEqual(await native.readFile(session.session_id, file), content)
    assert.deepEqual(await readFile(file), content)
    const diff = await native.reviewDiff(session.session_id)
    assert.equal(diff.pending_changes, 1)
    await native.reviewAbort(session.session_id)
    await assert.rejects(readFile(file))
    await native.stopSession(session.session_id)
    session = undefined
    console.log(JSON.stringify({ transport: 'native', mode, bytes: content.length }))
  } finally {
    if (session !== undefined) await native.stopSession(session.session_id).catch(() => {})
    await Promise.all([project, state, runtime].map(path => rm(path, { recursive: true, force: true })))
  }
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
