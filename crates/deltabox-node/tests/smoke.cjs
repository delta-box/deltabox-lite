#!/usr/bin/env node

const assert = require('node:assert/strict')
const { mkdtemp, readFile, realpath, rm } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [addonPath, helperBinary] = process.argv.slice(2)
  if (!addonPath || !helperBinary) throw new Error('usage: smoke.cjs <addon.node> <deltabox>')
  const { NativeDeltabox } = require(addonPath)
  const project = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-smoke-')))
  const state = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-state-')))
  const runtime = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-runtime-')))
  const file = join(project, 'native-buffer.txt')
  process.env.DELTABOX_NATIVE_SHARED_ENV = 'shared-from-node'
  const native = new NativeDeltabox(project, helperBinary, true, state, runtime)
  let session
  try {
    session = await native.startSession(undefined, 'fast')
    assert.equal(session.resolvedFsMode, 'cbcow-through')
    assert.equal(session.durability, 'process-crash')
    assert.ok(session.mountpoint)

    const exec = await native.exec(
      session.sessionId,
      ['/bin/sh', '-c', 'printf "%s:%s" "$DELTABOX_NATIVE_SHARED_ENV" "$EXPLICIT_VALUE"'],
      project,
      [{ key: 'EXPLICIT_VALUE', value: 'explicit' }],
      'native smoke',
    )
    assert.equal(exec.exitCode, 0)
    assert.equal(exec.stdout, 'shared-from-node:explicit')

    await native.beginReview(session.sessionId, 'task', 'native smoke')
    const content = Buffer.from([0, 1, 2, 253, 254, 255])
    await native.writeFile(session.sessionId, file, content)
    assert.deepEqual(await native.readFile(session.sessionId, file), content)
    assert.deepEqual(await readFile(file), content)
    const diff = await native.reviewDiff(session.sessionId)
    assert.equal(diff.pendingChanges, 1)
    await native.reviewAbort(session.sessionId)
    await native.stopSession(session.sessionId)
    session = undefined
    console.log(JSON.stringify({ transport: 'native', environment: 'shared', bytes: content.length }))
  } finally {
    if (session !== undefined) await native.stopSession(session.sessionId).catch(() => {})
    await Promise.all([project, state, runtime].map(path => rm(path, { recursive: true, force: true })))
  }
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
