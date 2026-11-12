#!/usr/bin/env node
// Actual native-client death, orphan adoption, and live-owner admission.
// node history-process-recovery.cjs <addon.node> <deltabox>
const assert = require('node:assert/strict')
const fs = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const { spawn } = require('node:child_process')

const [addon, helper, childMode, root, isolation = 'none'] = process.argv.slice(2)
const { NativeDeltabox } = require(addon)
const native = (work, mode = 'none') => new NativeDeltabox(join(work, 'project'), helper, {
  stateDir: join(work, 'state'), runtimeDir: join(work, 'runtime'),
  fsMode: 'cbcow-through', isolation: mode,
})

async function child() {
  const client = native(root, isolation)
  const sid = (await client.startSession()).session_id
  if (childMode === 'live' || childMode === 'abandon') {
    if (childMode === 'abandon') {
      await client.beginReview(sid, 'task', 'interrupted ordinary review')
      await client.writeFile(sid, join(root, 'project', 'a'), Buffer.from('uncommitted'))
    }
    process.send({ sid })
    await new Promise(resolve => process.once('message', resolve))
    await client.stopSession(sid)
    process.disconnect()
    return
  }
  const v1 = await client.historyCapture(sid, 'v1')
  await client.writeFile(sid, join(root, 'project', 'a'), Buffer.from('v2'))
  await client.writeFile(sid, join(root, 'project', 'b'), Buffer.from('v2'))
  await client.commit(sid)
  const v2 = await client.historyCapture(sid, 'v2')
  await fs.writeFile(join(root, 'versions.json'), JSON.stringify({ sid, v1, v2 }))
  await fs.writeFile(join(root, 'project', 'target', 'output'), 'new generated output')
  await client.historyRestore(sid, v1.id, v2.id)
  throw new Error('expected crash hook did not fire')
}

function launch(work, mode, childKind, crash) {
  const proc = spawn(process.execPath, [__filename, addon, helper, childKind, work, mode], {
    env: { ...process.env, ...(crash ? { DELTABOX_TEST_CRASH_AT: crash } : {}) },
    stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
  })
  let output = ''
  proc.stdout.on('data', chunk => { output += chunk })
  proc.stderr.on('data', chunk => { output += chunk })
  const exited = new Promise((resolve, reject) => {
    proc.once('error', reject)
    proc.once('exit', (code, signal) => resolve({ code, signal, output }))
  })
  return { proc, exited }
}

async function fixture() {
  const work = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'deltabox-history-process-')))
  await fs.mkdir(join(work, 'project'))
  await fs.writeFile(join(work, 'project', 'a'), 'v1')
  await fs.writeFile(join(work, 'project', 'b'), 'v1')
  await fs.writeFile(join(work, 'project', '.gitignore'), 'target/\n')
  await fs.mkdir(join(work, 'project', 'target'))
  await fs.writeFile(join(work, 'project', 'target', 'output'), 'old output')
  return work
}

async function main() {
  for (const mode of ['none', 'deltabox']) {
    for (const crash of ['prepared', 'files_renamed', 'files_host_1', 'host_applied', 'files_base_1', 'base_applied', 'files_before_commit', 'state_updated']) {
      const work = await fixture()
      let client, sid
      try {
        const dead = await launch(work, mode, 'crash', `macos_history_${crash}`).exited
        assert.equal(dead.signal, 'SIGABRT', dead.output)
        const { sid: oldSid, v1, v2 } = JSON.parse(await fs.readFile(join(work, 'versions.json'), 'utf8'))
        client = native(work, mode)
        assert.equal(await fs.readFile(join(work, 'project', 'target', 'output'), 'utf8'), 'new generated output')
        await assert.rejects(client.startSession(), /session orphaned/)
        assert.deepEqual(await client.orphanedSessions(), [oldSid])
        if (crash !== 'state_updated') {
          await fs.writeFile(join(work, 'project', 'target', 'output'), 'post-crash ignored edit')
          await assert.rejects(client.adoptSession(oldSid), /history recovery conflict/)
          assert.equal(await fs.readFile(join(work, 'project', 'target', 'output'), 'utf8'), 'post-crash ignored edit')
          assert.deepEqual(await client.orphanedSessions(), [oldSid])
          await fs.writeFile(join(work, 'project', 'target', 'output'), 'new generated output')
          const file = join(work, 'project', 'a')
          const original = await fs.readFile(file)
          await fs.writeFile(file, 'post-crash edit')
          await fs.writeFile(join(work, 'project', 'external'), 'new user file')
          await assert.rejects(client.adoptSession(oldSid), /history recovery conflict/)
          assert.equal(await fs.readFile(file, 'utf8'), 'post-crash edit')
          assert.equal(await fs.readFile(join(work, 'project', 'external'), 'utf8'), 'new user file')
          // Simulate explicit user reconciliation; adopt can now recover.
          await fs.writeFile(file, original)
          await fs.unlink(join(work, 'project', 'external'))
        }
        await client.adoptSession(oldSid)
        assert.equal((await client.releaseSession(oldSid)).status, 'stopped')
        sid = (await client.startSession()).session_id
        assert.notEqual(sid, oldSid)
        assert.equal((await client.sessionStatus(oldSid)).status, 'stopped')
        const accepted = crash === 'state_updated' ? v1 : v2
        assert.equal((await client.historyDiff(sid, accepted.id)).pending_changes, 0)
        await client.abort(sid)
        assert.equal((await client.historyDiff(sid, accepted.id)).pending_changes, 0)
        assert.equal(await fs.readFile(join(work, 'project', 'target', 'output'), 'utf8'), 'new generated output')
        const versions = await client.historyList(sid)
        assert.ok(versions.some(value => value.id === v1.id))
        assert.ok(versions.some(value => value.id === v2.id))
        assert.equal(versions.length, 3) // includes the durable pre-restore backup
      } finally {
        if (sid) await client.stopSession(sid)
        await fs.rm(work, { recursive: true, force: true })
      }
    }

    const work = await fixture()
    const live = launch(work, mode, 'live')
    try {
      const { sid: ownerSid } = await new Promise((resolve, reject) => {
        live.proc.once('message', resolve)
        live.exited.then(result => reject(new Error(`owner exited before ready: ${result.output}`)))
      })
      const contender = native(work, mode)
      await assert.rejects(contender.startSession(), /session busy/)
      assert.equal((await contender.sessionStatus(ownerSid)).status, 'running')
      assert.equal(await fs.readFile(join(work, 'project', 'a'), 'utf8'), 'v1')
      live.proc.send('stop')
      assert.equal((await live.exited).code, 0)
      const sid = (await contender.startSession()).session_id
      try {
        // A different coordinator in the SAME process is live as well.
        await assert.rejects(native(work, mode).startSession(), /session busy/)
      } finally {
        await contender.stopSession(sid)
      }
    } finally {
      if (live.proc.exitCode === null && live.proc.signalCode === null) {
        live.proc.kill('SIGKILL')
        await live.exited
      }
      await fs.rm(work, { recursive: true, force: true })
    }

    const abandonedWork = await fixture()
    const abandoned = launch(abandonedWork, mode, 'abandon')
    let recovered, recoveredSid
    try {
      const { sid: orphanSid } = await new Promise((resolve, reject) => {
        abandoned.proc.once('message', resolve)
        abandoned.exited.then(result => reject(new Error(`owner exited before ready: ${result.output}`)))
      })
      abandoned.proc.kill('SIGKILL')
      await abandoned.exited
      await fs.writeFile(join(abandonedWork, 'project', 'external'), 'post-crash edit')
      recovered = native(abandonedWork, mode)
      await assert.rejects(recovered.startSession(), /session orphaned/)
      assert.deepEqual(await recovered.orphanedSessions(), [orphanSid])
      await recovered.adoptSession(orphanSid)
      await recovered.reviewCommit(orphanSid)
      await recovered.releaseSession(orphanSid)
      recoveredSid = (await recovered.startSession()).session_id
      assert.equal(await fs.readFile(join(abandonedWork, 'project', 'a'), 'utf8'), 'uncommitted')
      assert.equal(await fs.readFile(join(abandonedWork, 'project', 'external'), 'utf8'), 'post-crash edit')
      await recovered.abort(recoveredSid)
      assert.equal(await fs.readFile(join(abandonedWork, 'project', 'a'), 'utf8'), 'uncommitted')
    } finally {
      if (recoveredSid) await recovered.stopSession(recoveredSid)
      if (abandoned.proc.exitCode === null && abandoned.proc.signalCode === null) {
        abandoned.proc.kill('SIGKILL')
        await abandoned.exited
      }
      await fs.rm(abandonedWork, { recursive: true, force: true })
    }
  }
  console.log(JSON.stringify({ historyProcessRecovery: 'passed', crashBoundaries: 16, liveOwnerRefused: true }))
}

(childMode ? child() : main()).catch(error => { console.error(error); process.exitCode = 1 })
