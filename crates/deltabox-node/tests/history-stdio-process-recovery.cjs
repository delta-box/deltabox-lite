#!/usr/bin/env node
// node history-stdio-process-recovery.cjs <deltabox>
const assert = require('node:assert/strict')
const { spawn, execFileSync } = require('node:child_process')
const { createInterface } = require('node:readline')
const fs = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const [binary] = process.argv.slice(2)

function serve(work, crash) {
  const env = { ...process.env, DELTABOX_STATE_DIR: join(work, 'state'), ...(crash ? { DELTABOX_TEST_CRASH_AT: crash } : {}) }
  const child = spawn(binary, ['serve'], { cwd: join(work, 'project'), env })
  const pending = new Map()
  let sequence = 0
  let stderr = ''
  child.stderr.on('data', chunk => { stderr += chunk })
  createInterface({ input: child.stdout }).on('line', line => {
    const response = JSON.parse(line)
    const call = pending.get(response.id)
    if (!call) return
    pending.delete(response.id)
    if (response.error) call.reject(Object.assign(new Error(response.error.message), { data: response.error.data }))
    else call.resolve(response.result)
  })
  const exited = new Promise((resolve, reject) => {
    child.once('error', reject)
    child.once('exit', (code, signal) => {
      for (const call of pending.values()) call.reject(new Error(`server exited ${code}/${signal}: ${stderr}`))
      resolve({ code, signal })
    })
  })
  const rpc = (method, params) => new Promise((resolve, reject) => {
    const id = ++sequence
    pending.set(id, { resolve, reject })
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`)
  })
  return { child, exited, rpc, close: async () => { child.stdin.end(); await exited } }
}

async function main() {
  for (const crash of ['prepared', 'host_applied', 'base_applied', 'state_updated']) {
    const work = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'deltabox-stdio-crash-')))
    const project = join(work, 'project')
    await fs.mkdir(project)
    await fs.writeFile(join(project, 'a'), 'v1')
    const start = { project, fs_mode: 'cbcow-through', require_fs_mode: true }
    const first = serve(work, `macos_history_${crash}`)
    let next, sid
    try {
      const old = (await first.rpc('session.start', start)).session_id
      const v1 = await first.rpc('history.capture', { session_id: old })
      await first.rpc('fs.write', { session_id: old, path: join(project, 'a'), content_base64: Buffer.from('v2').toString('base64') })
      await first.rpc('changes.commit', { session_id: old })
      const v2 = await first.rpc('history.capture', { session_id: old })
      await assert.rejects(first.rpc('history.restore', { session_id: old, version_id: v1.id, expected_version_id: v2.id }), /server exited/)
      assert.equal((await first.exited).signal, 'SIGABRT')
      next = serve(work)
      await assert.rejects(next.rpc('session.start', start), /session busy/)
      if (crash !== 'state_updated') {
        await fs.writeFile(join(project, 'external'), 'post-crash edit')
        await assert.rejects(next.rpc('session.release', { session_id: old }), error => {
          assert.match(error.message, /history recovery conflict/)
          assert.equal(error.data.type, 'project-conflict')
          return true
        })
        assert.equal(await fs.readFile(join(project, 'external'), 'utf8'), 'post-crash edit')
        await fs.unlink(join(project, 'external'))
      }
      assert.equal((await next.rpc('session.release', { session_id: old })).status, 'stopped')
      sid = (await next.rpc('session.start', start)).session_id
      const expected = crash === 'state_updated' ? v1 : v2
      assert.equal((await next.rpc('history.diff', { session_id: sid, version_id: expected.id })).pending_changes, 0)
      assert.equal((await next.rpc('history.list', { session_id: sid })).length, 3)
      const liveContender = serve(work)
      try {
        await assert.rejects(liveContender.rpc('session.start', start), /session busy/)
      } finally { await liveContender.close() }
      await next.rpc('session.stop', { session_id: sid })
      sid = undefined

      // One-shot CLI start remains persistent after its creator exits.
      const env = { ...process.env, DELTABOX_STATE_DIR: join(work, 'state') }
      const legacy = JSON.parse(execFileSync(binary, ['session', 'start', project, '--fs-mode', 'cbcow-through', '--require-fs-mode', '--json'], { env, encoding: 'utf8' }))
      try { await assert.rejects(next.rpc('session.start', start), /session busy/) }
      finally { execFileSync(binary, ['session', 'stop', legacy.session_id, '--project', project], { env }) }
    } finally {
      if (sid) await next.rpc('session.stop', { session_id: sid }).catch(() => {})
      if (next) await next.close()
      if (first.child.exitCode === null && first.child.signalCode === null) first.child.kill('SIGKILL')
      await first.exited
      await fs.rm(work, { recursive: true, force: true })
    }
  }
  console.log(JSON.stringify({ stdioProcessRecovery: 'passed', crashBoundaries: 4, liveOwnerRefused: true, persistentCliPreserved: true }))
}
main().catch(error => { console.error(error); process.exitCode = 1 })
