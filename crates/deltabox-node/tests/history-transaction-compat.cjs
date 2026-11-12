#!/usr/bin/env node
// Verify that an older runtime fails closed on an interrupted new transaction.
// node history-transaction-compat.cjs <new-addon> <new-helper> <old-addon> <old-helper>
const fs = require('node:fs/promises')
const assert = require('node:assert/strict')
const { spawn } = require('node:child_process')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const [addon, helper, oldAddon, oldHelper, mode, root] = process.argv.slice(2)
async function child() {
  const { NativeDeltabox } = require(mode === 'old' ? oldAddon : addon)
  const client = new NativeDeltabox(join(root, 'project'), mode === 'old' ? oldHelper : helper, {
    stateDir: join(root, 'state'), runtimeDir: join(root, 'runtime'), fsMode: 'cbcow-through', isolation: 'none',
  })
  if (mode === 'old') {
    const savedSid = (await fs.readFile(join(root, 'sid'), 'utf8')).trim()
    await assert.rejects(client.sessionStatus(savedSid), /unknown variant.*history-files-v1/)
    assert.equal(await fs.readFile(join(root, 'project', 'a'), 'utf8'), 'v2')
    return
  }
  if (mode === 'recover') {
    const savedSid = (await fs.readFile(join(root, 'sid'), 'utf8')).trim()
    await client.adoptSession(savedSid)
    await client.releaseSession(savedSid)
    const sid = (await client.startSession()).session_id
    assert.equal(await fs.readFile(join(root, 'project', 'a'), 'utf8'), 'v2')
    await client.abort(sid)
    assert.equal(await fs.readFile(join(root, 'project', 'a'), 'utf8'), 'v2')
    await client.releaseSession(sid)
    return
  }
  const sid = (await client.startSession()).session_id
  await fs.writeFile(join(root, 'sid'), sid)
  const v1 = await client.historyCapture(sid, 'v1')
  await client.writeFile(sid, join(root, 'project', 'a'), Buffer.from('v2'))
  await client.commit(sid)
  const v2 = await client.historyCapture(sid, 'v2')
  await client.historyRestore(sid, v1.id, v2.id)
  throw new Error('expected debug crash hook')
}
function run(kind, work) {
  return new Promise((resolve, reject) => {
    const proc = spawn(process.execPath, [__filename, addon, helper, oldAddon, oldHelper, kind, work], {
      env: { ...process.env, ...(kind === 'crash' ? { DELTABOX_TEST_CRASH_AT: 'macos_history_prepared' } : {}) },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let text = ''
    proc.stdout.on('data', chunk => { text += chunk }); proc.stderr.on('data', chunk => { text += chunk })
    proc.on('error', reject); proc.on('exit', (code, signal) => resolve({ code, signal, text }))
  })
}
async function main() {
  const work = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'history-transaction-compat-')))
  try {
    await fs.mkdir(join(work, 'project'))
    await fs.writeFile(join(work, 'project', 'a'), 'v1')
    const crash = await run('crash', work); assert.equal(crash.signal, 'SIGABRT', crash.text)
    const old = await run('old', work); assert.equal(old.code, 0, old.text)
    const recovered = await run('recover', work); assert.equal(recovered.code, 0, recovered.text)
    console.log(JSON.stringify({ oldRuntimeRefusesNewTransaction: true, currentFilesPreserved: true, newRuntimeRecovery: true }))
  } finally { await fs.rm(work, { recursive: true, force: true }) }
}
(mode ? child() : main()).catch(error => { console.error(error); process.exitCode = 1 })
