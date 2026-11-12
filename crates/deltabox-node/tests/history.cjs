#!/usr/bin/env node
// Real N-API integration: node history.cjs <addon.node> <deltabox> [none|deltabox]
const assert = require('node:assert/strict')
const fs = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [addon, helper, isolation = 'none'] = process.argv.slice(2)
  if (!addon || !helper) throw new Error('usage: history.cjs <addon.node> <deltabox> [none|deltabox]')
  const { NativeDeltabox } = require(addon)
  const work = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'deltabox-history-test-')))
  const project = join(work, 'project')
  const stateDir = join(work, 'state')
  await fs.mkdir(project)
  await fs.writeFile(join(project, 'a.txt'), 'v1')
  await fs.writeFile(join(project, '.gitignore'), 'target/\n*.log\n.env\n')
  await fs.mkdir(join(project, 'target'))
  await fs.writeFile(join(project, 'target', 'build'), 'old output')
  const options = { stateDir, runtimeDir: join(work, 'runtime'), fsMode: 'cbcow-through', isolation }
  let native = new NativeDeltabox(project, helper, options)
  let sid
  try {
    sid = (await native.startSession()).session_id
    assert.deepEqual(await native.historyCapabilities(sid), { version: 1, supported: true, retention: 'manual', scope: 'project' })
    const v1 = await native.historyCapture(sid, 'v1')
    await assert.rejects(native.historyList('../escape'), /canonical UUID/)
    if (isolation === 'deltabox') {
      const namespace = join(stateDir, 'macos-native', 'history-v1')
      const [key] = await fs.readdir(namespace)
      const immutable = join(namespace, key, v1.id, 'tree', 'a.txt')
      const blocked = await native.exec(sid, ['/bin/sh', '-c', 'printf corrupt > "$1"', 'history-test', immutable], project, undefined, 'history isolation', 'read-write')
      assert.notEqual(blocked.exit_code, 0)
      assert.equal(await fs.readFile(immutable, 'utf8'), 'v1')
    }

    await native.beginReview(sid, 'task', 'v2')
    await native.writeFile(sid, join(project, 'a.txt'), Buffer.from('v2'))
    const v2 = await native.historyCapture(sid, 'v2 captured during review')
    await assert.rejects(native.historyRestore(sid, v1.id, v2.id), /active review/)
    await native.reviewCommit(sid)
    assert.equal((await native.historyList(sid)).length, 2)
    assert.equal((await native.historyDiff(sid, v1.id)).changes[0].path, 'a.txt')
    await fs.writeFile(join(project, 'a.txt'), 'external')
    await assert.rejects(native.historyRestore(sid, v1.id, v2.id), /history restore conflict/)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'external')
    // Host changes can be explicitly captured, making the new expected state reviewable.
    const external = await native.historyCapture(sid, 'external')
    await fs.writeFile(join(project, 'target', 'build'), 'new output')
    await fs.writeFile(join(project, '.env'), 'keep private settings')
    const restored = await native.historyRestore(sid, v1.id, external.id)
    assert.equal(await fs.readFile(join(project, 'target', 'build'), 'utf8'), 'new output')
    assert.equal(await fs.readFile(join(project, '.env'), 'utf8'), 'keep private settings')
    assert.equal(restored.version.id, v1.id)
    assert.notEqual(restored.backup.id, external.id)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v1')
    await native.abort(sid)
    assert.equal(await fs.readFile(join(project, 'target', 'build'), 'utf8'), 'new output')
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v1')
    await native.beginReview(sid, 'task', 'aborted review')
    await native.writeFile(sid, join(project, 'a.txt'), Buffer.from('review draft'))
    const draft = await native.historyCapture(sid, 'draft')
    await native.reviewAbort(sid)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v1')
    assert.ok((await native.historyList(sid)).some(v => v.id === draft.id))
    await native.stopSession(sid)
    sid = undefined
    // New coordinator and session reuse history by canonical project + stable stateDir.
    native = new NativeDeltabox(project, helper, options)
    const stateBeforePreview = (await fs.readdir(join(stateDir, 'macos-native'))).sort()
    assert.equal((await native.historyPreview(v2.id)).changes[0].path, 'a.txt')
    await assert.rejects(native.historyPreview('../escape'), /canonical UUID/)
    assert.deepEqual((await fs.readdir(join(stateDir, 'macos-native'))).sort(), stateBeforePreview)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v1')
    sid = (await native.startSession()).session_id
    assert.ok((await native.historyList(sid)).some(v => v.id === v2.id))
    await native.historyRestore(sid, v2.id, v1.id)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v2')
    // Capture waits for an active command, including in embedded_fast mode.
    const marker = join(project, 'command-started')
    const command = native.exec(sid, ['/bin/sh', '-c', 'printf partial > "$1"; : > "$2"; sleep 0.2; printf v2 > "$1"; rm "$2"', 'quiescence', join(project, 'a.txt'), marker], project)
    const deadline = Date.now() + 5000
    while (!(await fs.stat(marker).catch(() => null))) {
      if (Date.now() >= deadline) throw new Error('test command never started')
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    const boundary = await native.historyCapture(sid, 'after concurrent command')
    assert.equal((await command).exit_code, 0)
    assert.equal((await native.historyDiff(sid, boundary.id)).changes.length, 0)
    assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v2')
    // A type swap must fail before modifying live files when it contains excluded data.
    await fs.unlink(join(project, 'a.txt'))
    await fs.mkdir(join(project, 'a.txt'))
    await fs.writeFile(join(project, 'a.txt', 'output.log'), 'do not delete')
    const collision = await native.historyCapture(sid, 'collision')
    await assert.rejects(native.historyRestore(sid, v1.id, collision.id), /excluded data/)
    assert.equal(await fs.readFile(join(project, 'a.txt', 'output.log'), 'utf8'), 'do not delete')
    await native.releaseSession(sid)
    sid = undefined
    assert.equal(await fs.readFile(join(project, 'a.txt', 'output.log'), 'utf8'), 'do not delete')
    native = new NativeDeltabox(project, helper, { ...options, fsMode: 'copy-private' })
    sid = (await native.startSession()).session_id
    assert.equal((await native.historyCapabilities(sid)).supported, false)
    await assert.rejects(native.historyCapture(sid), /history v1 requires macOS cbcow-through/)
    await native.stopSession(sid)
    sid = undefined
    console.log(JSON.stringify({ history: 'passed', isolation, restart: true, conflict: true, reviewAbort: true, coldPreviewWithoutSession: true }))
  } finally {
    if (sid) await native.stopSession(sid).catch(() => {})
    await fs.rm(work, { recursive: true, force: true })
  }
}
main().catch(error => { console.error(error); process.exitCode = 1 })
