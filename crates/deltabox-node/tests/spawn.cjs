#!/usr/bin/env node

const assert = require('node:assert/strict')
const { mkdtemp, realpath, rm } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function runForIsolation(NativeDeltabox, helperBinary, isolation) {
  const project = await realpath(await mkdtemp(join(tmpdir(), `deltabox-spawn-proj-${isolation}-`)))
  const state = await realpath(await mkdtemp(join(tmpdir(), `deltabox-spawn-state-${isolation}-`)))
  const runtime = await realpath(await mkdtemp(join(tmpdir(), `deltabox-spawn-rt-${isolation}-`)))
  const native = new NativeDeltabox(project, helperBinary, {
    stateDir: state,
    runtimeDir: runtime,
    fsMode: 'cbcow-through',
    isolation,
  })
  let session
  try {
    session = await native.startSession(undefined, 'fast', undefined, 'auto', false)

    // 1. Live streaming stdout and stderr before wait() resolves.
    const streamedOut = []
    const streamedErr = []
    let sawFirstBeforeExit = false
    let waitedDone = false
    const spawned = native.spawn(session.session_id, {
      argv: ['/bin/sh', '-c', 'printf "first\\n"; sleep 0.25; printf "second\\n"; printf "err-line\\n" >&2'],
      cwd: project,
      label: `spawn stream (${isolation})`,
    })
    spawned.onStdout((chunk) => {
      const text = chunk.toString('utf8')
      streamedOut.push(text)
      if (!waitedDone && text.includes('first')) sawFirstBeforeExit = true
    })
    spawned.onStderr((chunk) => {
      streamedErr.push(chunk.toString('utf8'))
    })
    const exit = await spawned.wait()
    waitedDone = true
    spawned.close()
    assert.equal(exit.exit_code, 0)
    assert.equal(streamedOut.join(''), 'first\nsecond\n')
    assert.equal(streamedErr.join(''), 'err-line\n')
    assert.equal(sawFirstBeforeExit, true, `expected incremental stdout before wait() in ${isolation} mode`)

    // 2. Background descendant inheriting stdout/stderr must not block spawn() or exec().
    const bgStart = Date.now()
    const bgSpawn = native.spawn(session.session_id, {
      argv: ['/bin/sh', '-c', 'sleep 30 & echo started'],
      cwd: project,
      label: `spawn background (${isolation})`,
    })
    const bgOut = []
    bgSpawn.onStdout((chunk) => bgOut.push(chunk.toString('utf8')))
    const bgExit = await bgSpawn.wait()
    bgSpawn.close()
    assert.equal(bgExit.exit_code, 0)
    assert.equal(bgOut.join(''), 'started\n')
    assert.ok(Date.now() - bgStart < 5000, `spawn() hung on background child in ${isolation} mode`)

    const bgExecStart = Date.now()
    const bgExec = await native.exec(
      session.session_id,
      ['/bin/sh', '-c', 'sleep 30 & echo started'],
      project,
      undefined,
      `exec background (${isolation})`,
    )
    assert.equal(bgExec.exit_code, 0)
    assert.equal(bgExec.stdout, 'started\n')
    assert.ok(Date.now() - bgExecStart < 5000, `exec() hung on background child in ${isolation} mode`)

    // 3. Handle-based kill() terminates a running command promptly.
    const killStart = Date.now()
    const toKill = native.spawn(session.session_id, {
      argv: ['/bin/sh', '-c', 'echo ready; sleep 60'],
      cwd: project,
      label: `spawn kill (${isolation})`,
    })
    toKill.onStdout((chunk) => {
      if (chunk.toString('utf8').includes('ready')) toKill.kill()
    })
    const killOutcome = await toKill.wait().then(
      (res) => ({ ok: true, res }),
      (err) => ({ ok: false, err }),
    )
    toKill.close()
    assert.ok(Date.now() - killStart < 5000, `kill() did not end command promptly in ${isolation} mode`)
    if (killOutcome.ok) {
      assert.notEqual(killOutcome.res.exit_code, 0)
    }

    await native.stopSession(session.session_id)
    session = undefined
  } finally {
    if (session !== undefined) await native.stopSession(session.session_id).catch(() => {})
    await Promise.all([project, state, runtime].map((p) => rm(p, { recursive: true, force: true })))
  }
}

async function main() {
  const [addonPath, helperBinary] = process.argv.slice(2)
  if (!addonPath || !helperBinary) {
    throw new Error('usage: spawn.cjs <addon.node> <deltabox>')
  }
  const { NativeDeltabox } = require(addonPath)
  await runForIsolation(NativeDeltabox, helperBinary, 'none')
  await runForIsolation(NativeDeltabox, helperBinary, 'deltabox')
  console.log(JSON.stringify({ spawn: 'ok', modes: ['none', 'deltabox'] }))
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
