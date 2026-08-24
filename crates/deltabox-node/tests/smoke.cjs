#!/usr/bin/env node

const assert = require('node:assert/strict')
const { mkdtemp, readFile, realpath, rm, stat } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [addonPath, helperBinary, mode = 'cbcow-through', requestedIsolation] = process.argv.slice(2)
  if (!addonPath || !helperBinary || !['cbcow-through', 'git'].includes(mode)) {
    throw new Error('usage: smoke.cjs <addon.node> <deltabox> [cbcow-through|git] [bubblewrap|docker]')
  }
  const { NativeDeltabox } = require(addonPath)
  const project = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-smoke-')))
  const state = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-state-')))
  const runtime = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-runtime-')))
  const file = join(project, 'native-buffer.txt')
  process.env.DELTABOX_NATIVE_SHARED_ENV = 'shared-from-node'
  const gitMode = mode === 'git'
  const isolation = requestedIsolation || (gitMode ? 'bubblewrap' : 'none')
  if ((gitMode && !['bubblewrap', 'docker'].includes(isolation)) || (!gitMode && isolation !== 'none')) {
    throw new Error(`unsupported smoke mode ${mode}+${isolation}`)
  }
  const native = new NativeDeltabox(project, helperBinary, {
    stateDir: state,
    runtimeDir: runtime,
    fsMode: mode,
    isolation,
  })
  let session
  try {
    session = await native.startSession(undefined, 'fast')
    assert.equal(session.resolved_fs_mode, mode)
    assert.equal(session.durability, 'process-crash')
    assert.equal(session.mountpoint === undefined, gitMode)
    assert.equal(session.isolation, isolation)
    assert.equal(session.isolation_provider, gitMode ? isolation : undefined)
    assert.equal(session.rollback_guarantee, gitMode ? 'git-tracked-files' : 'fuse-routed-writes-only')

    await native.beginReview(session.session_id, 'task', 'native smoke')
    const command = isolation === 'docker'
      ? ['/bin/sh', '-c', 'printf "%s:%s" "$DELTABOX_NATIVE_SHARED_ENV" "$EXPLICIT_VALUE"; printf "container\n" > docker-command.txt; touch /etc/deltabox-ro-test 2>/dev/null && exit 9; printf ":%s" "$PWD"']
      : ['/bin/sh', '-c', 'printf "%s:%s" "$DELTABOX_NATIVE_SHARED_ENV" "$EXPLICIT_VALUE"']
    const exec = await native.exec(
      session.session_id,
      command,
      project,
      [{ key: 'EXPLICIT_VALUE', value: 'explicit' }],
      'native smoke',
    )
    assert.equal(exec.exit_code, 0)
    assert.equal(exec.stdout, isolation === 'docker'
      ? ':explicit:/workspace'
      : gitMode ? ':explicit' : 'shared-from-node:explicit')
    if (isolation === 'docker') {
      const commandFile = join(project, 'docker-command.txt')
      assert.equal(await readFile(commandFile, 'utf8'), 'container\n')
      assert.equal((await stat(commandFile)).uid, process.getuid())
    }

    const content = Buffer.from([0, 1, 2, 253, 254, 255])
    await native.writeFile(session.session_id, file, content)
    assert.deepEqual(await native.readFile(session.session_id, file), content)
    assert.deepEqual(await readFile(file), content)
    const diff = await native.reviewDiff(session.session_id)
    assert.equal(diff.pending_changes, isolation === 'docker' ? 2 : 1)
    await native.reviewCommit(session.session_id)
    assert.deepEqual(await readFile(file), content)

    await native.beginReview(session.session_id, 'task', 'native smoke abort')
    await native.writeFile(session.session_id, file, Buffer.from('replacement'))
    assert.equal((await native.reviewDiff(session.session_id)).pending_changes, 1)
    await native.reviewAbort(session.session_id)
    assert.deepEqual(await readFile(file), content)
    await native.stopSession(session.session_id)
    session = undefined
    console.log(JSON.stringify({ transport: 'native', mode, isolation, bytes: content.length }))
  } finally {
    if (session !== undefined) await native.stopSession(session.session_id).catch(() => {})
    await Promise.all([project, state, runtime].map(path => rm(path, { recursive: true, force: true })))
  }
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
