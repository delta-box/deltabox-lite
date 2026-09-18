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
  const native = new NativeDeltabox(project, helperBinary, {
    stateDir: state,
    runtimeDir: runtime,
    fsMode: 'cbcow-through',
    isolation: 'none',
  })
  let session
  try {
    session = await native.startSession(undefined, 'fast', undefined, 'auto', false)
    assert.equal(session.resolved_fs_mode, 'cbcow-through')
    assert.equal(session.durability, 'process-crash')
    assert.equal(session.status, 'running')
    assert.equal(session.isolation, 'none')
    assert.equal(session.isolation_provider, null)
    assert.equal(session.state_provider, 'macos-apfs-fsevents')
    assert.equal(session.fallback_reason, null)
    assert.ok(session.mountpoint)
    assert.equal(await native.reviewStatus(session.session_id), null)

    const baseline = await native.checkpoint(session.session_id, 'baseline')
    assert.equal(baseline.label, 'baseline')

    const exec = await native.exec(
      session.session_id,
      ['/bin/sh', '-c', 'printf "%s:%s" "$DELTABOX_NATIVE_SHARED_ENV" "$EXPLICIT_VALUE"'],
      project,
      [{ key: 'EXPLICIT_VALUE', value: 'explicit' }],
      'native smoke',
    )
    assert.equal(exec.exit_code, 0)
    assert.equal(exec.stdout, 'shared-from-node:explicit')

    const review = await native.beginReview(session.session_id, 'task', 'native smoke')
    assert.equal((await native.reviewStatus(session.session_id)).kind, 'task')
    const content = Buffer.from([0, 1, 2, 253, 254, 255])
    await native.writeFile(session.session_id, file, content)
    assert.deepEqual(await native.readFile(session.session_id, file), content)
    assert.deepEqual(await readFile(file), content)
    const diff = await native.reviewDiff(session.session_id)
    assert.equal(diff.pending_changes, 1)
    assert.equal((await native.diff(session.session_id, baseline.id)).pending_changes, 1)
    assert.equal((await native.reviewSkip(session.session_id)).checkpoint_id, review.checkpoint_id)
    await native.reviewAbort(session.session_id)

    const generic = await native.checkpoint(session.session_id, 'generic abort')
    await native.writeFile(session.session_id, file, content)
    assert.equal((await native.abort(session.session_id, generic.id)).restored, 1)

    const accepted = await native.checkpoint(session.session_id, 'generic commit')
    await native.writeFile(session.session_id, file, content)
    const committed = await native.commit(session.session_id, accepted.id)
    assert.equal(typeof committed.checkpoint_id, 'string')
    assert.equal(Number.isInteger(committed.applied), true)
    assert.equal((await native.sessionStatus(session.session_id)).status, 'running')
    await native.stopSession(session.session_id)
    session = undefined
    console.log(JSON.stringify({ transport: 'native', environment: 'shared', bytes: content.length }))
  } finally {
    if (session !== undefined) await native.stopSession(session.session_id).catch(() => {})
    await Promise.all([project, state, runtime].map(path => rm(path, { recursive: true, force: true })))
  }
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
