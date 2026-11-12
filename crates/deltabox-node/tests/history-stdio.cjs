#!/usr/bin/env node
// node history-stdio.cjs <deltabox>
const assert = require('node:assert/strict')
const { spawn } = require('node:child_process')
const { createInterface } = require('node:readline')
const fs = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [binary] = process.argv.slice(2)
  if (!binary) throw new Error('usage: history-stdio.cjs <deltabox>')
  const work = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'deltabox-history-stdio-')))
  const project = join(work, 'project')
  await fs.mkdir(project)
  await fs.writeFile(join(project, 'a'), 'v1')
  const child = spawn(binary, ['serve'], { cwd: project, env: { ...process.env, DELTABOX_STATE_DIR: join(work, 'state') } })
  const pending = new Map()
  let sequence = 0
  const lines = createInterface({ input: child.stdout })
  lines.on('line', line => {
    const response = JSON.parse(line)
    const call = pending.get(response.id)
    if (!call) return
    pending.delete(response.id)
    if (response.error) call.reject(Object.assign(new Error(response.error.message), { data: response.error.data }))
    else call.resolve(response.result)
  })
  child.on('exit', code => {
    for (const call of pending.values()) call.reject(new Error(`server exited ${code}`))
  })
  child.stderr.pipe(process.stderr)
  const rpc = (method, params) => new Promise((resolve, reject) => {
    const id = ++sequence
    pending.set(id, { resolve, reject })
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`)
  })
  let sid
  try {
    sid = (await rpc('session.start', { project, fs_mode: 'cbcow-through', require_fs_mode: true })).session_id
    assert.equal((await rpc('history.capabilities', { session_id: sid })).supported, true)
    const v1 = await rpc('history.capture', { session_id: sid, label: 'v1' })
    await rpc('fs.write', { session_id: sid, path: join(project, 'a'), content_base64: Buffer.from('v2').toString('base64') })
    await rpc('changes.commit', { session_id: sid })
    const v2 = await rpc('history.capture', { session_id: sid })
    assert.equal((await rpc('history.diff', { session_id: sid, version_id: v1.id })).changes.length, 1)
    await fs.writeFile(join(project, 'a'), 'outside')
    await assert.rejects(rpc('history.restore', { session_id: sid, version_id: v1.id, expected_version_id: v2.id }), error => {
      assert.equal(error.data.type, 'project-conflict')
      assert.equal(error.data.conflicts[0].path, 'a')
      return true
    })
    const external = await rpc('history.capture', { session_id: sid })
    const result = await rpc('history.restore', { session_id: sid, version_id: v1.id, expected_version_id: external.id })
    assert.equal(result.version.id, v1.id)
    assert.ok(result.backup.id)
    assert.equal((await rpc('history.list', { session_id: sid })).length, 4)
    await rpc('session.stop', { session_id: sid })
    sid = undefined
    assert.equal((await rpc('history.preview', { version_id: v2.id })).changes.length, 1)
    await assert.rejects(rpc('history.preview', { version_id: '../escape' }), /canonical UUID/)
    assert.equal(await fs.readFile(join(project, 'a'), 'utf8'), 'v1')
    console.log(JSON.stringify({ history: 'passed', transport: 'stdio', structuredConflict: true }))
  } finally {
    if (sid) await rpc('session.stop', { session_id: sid }).catch(() => {})
    child.stdin.end()
    lines.close()
    child.kill()
    await fs.rm(work, { recursive: true, force: true })
  }
}
main().catch(error => { console.error(error); process.exitCode = 1 })
