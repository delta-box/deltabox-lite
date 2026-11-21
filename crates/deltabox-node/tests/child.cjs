#!/usr/bin/env node
// End-to-end test of the npm package's spawn()/exec() wrapper over the native
// addon. Runs unchanged under Node and Bun:
//   node tests/child.cjs <addon.node> <deltabox> [none|deltabox ...]
//   bun  tests/child.cjs <addon.node> <deltabox> [none|deltabox ...]

const assert = require('node:assert/strict')
const fs = require('node:fs')
const { mkdtemp, realpath, rm } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const { spawn, exec } = require('../../../npm/deltabox-lite/child.cjs')

const runtime = typeof Bun !== 'undefined' ? `bun ${Bun.version}` : `node ${process.versions.node}`
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const collect = (stream) =>
  new Promise((resolve, reject) => {
    const chunks = []
    stream.on('data', (chunk) => chunks.push(chunk))
    stream.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')))
    stream.on('error', reject)
  })
const running = (needle) => {
  try {
    return require('node:child_process').execFileSync('pgrep', ['-f', needle]).length > 0
  } catch {
    return false
  }
}
const closed = (stream) => new Promise((resolve) => (stream.closed ? resolve() : stream.once('close', resolve)))

// Open descriptors of this process, to prove nothing leaks or is closed twice.
const fdTargets = () =>
  fs.existsSync('/proc/self/fd')
    ? Object.fromEntries(
        fs.readdirSync('/proc/self/fd').map((fd) => {
          try {
            return [fd, fs.readlinkSync(`/proc/self/fd/${fd}`)]
          } catch {
            return [fd, '?']
          }
        }),
      )
    : null

async function withSession(NativeDeltabox, helper, isolation, body) {
  const project = await realpath(await mkdtemp(join(tmpdir(), `dbx-child-${isolation}-`)))
  const state = await realpath(await mkdtemp(join(tmpdir(), `dbx-child-state-`)))
  const runtimeDir = await realpath(await mkdtemp(join(tmpdir(), `dbx-child-rt-`)))
  const native = new NativeDeltabox(project, helper, { stateDir: state, runtimeDir, fsMode: 'cbcow-through', isolation })
  const session = await native.startSession(undefined, 'fast', undefined, 'auto', false)
  try {
    await body(native, session.session_id, project)
  } finally {
    await native.stopSession(session.session_id).catch(() => {})
    await Promise.all([project, state, runtimeDir].map((p) => rm(p, { recursive: true, force: true })))
  }
}

const cases = {
  async 'streams output before the command finishes'(native, sid) {
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', 'echo first; sleep 0.5; echo second; echo warn >&2'] })
    let finished = false
    child.result.then(() => (finished = true))
    const first = await new Promise((resolve) => child.stdout.once('data', resolve))
    assert.equal(first.toString(), 'first\n')
    assert.equal(finished, false, 'the first chunk must arrive while the command still runs')
    assert.ok(child.pid > 0, 'pid is known while running')
    const [rest, err, result] = await Promise.all([collect(child.stdout), collect(child.stderr), child.result])
    assert.equal(rest, 'second\n')
    assert.equal(err, 'warn\n')
    assert.equal(result.exit_code, 0)
    assert.equal(result.stdout, '', 'streamed output is not also buffered')
  },

  async 'exit then close, like ChildProcess'(native, sid) {
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', 'echo hi; exit 3'] })
    const events = []
    child.on('exit', (code) => events.push(['exit', code]))
    const done = new Promise((resolve) => child.on('close', (code) => resolve(events.push(['close', code]))))
    child.stdout.resume()
    child.stderr.resume()
    await done
    assert.deepEqual(events, [['exit', 3], ['close', 3]])
    assert.equal(child.exitCode, 3)
  },

  async 'a background child neither holds the streams open nor survives'(native, sid, isolation) {
    const started = Date.now()
    const tag = `sleep 30.${process.pid}`
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', `${tag} & echo started`] })
    const [out, result] = await Promise.all([collect(child.stdout), child.result])
    assert.equal(out, 'started\n')
    assert.ok(Date.now() - started < 10000, 'the background sleep held the command open')
    assert.equal(running(tag), false, 'the background sleep outlived its command')
    // In the Linux sandbox a child still starting up when the main process
    // exits loses its syscall supervisor first and dies by itself, so the
    // count can be 0; without a sandbox it is always stopped and counted.
    if (isolation === 'none') assert.equal(result.background_terminated, 1)
    else assert.ok(result.background_terminated <= 1)
  },

  async 'backpressure: a slow reader slows the command, nothing piles up'(native, sid) {
    const size = 32 * 1024 * 1024
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', `head -c ${size} /dev/zero`] })
    const base = process.memoryUsage().rss
    let bytes = 0
    let peak = 0
    child.stdout.on('data', (chunk) => {
      bytes += chunk.length
      child.stdout.pause()
      peak = Math.max(peak, process.memoryUsage().rss - base)
      setTimeout(() => child.stdout.resume(), 1)
    })
    child.stderr.resume()
    await Promise.all([closed(child.stdout), child.result])
    assert.equal(bytes, size)
    assert.ok(peak < 128 * 1024 * 1024, `buffered ${peak >> 20} MiB for a slow reader`)
  },

  async 'awaiting only result drains unread output'(native, sid) {
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', 'head -c 16777216 /dev/zero; echo done >&2'] })
    const result = await child.result
    assert.equal(result.exit_code, 0)
    await Promise.all([closed(child.stdout), closed(child.stderr)])
  },

  async 'kill() ends the command and its streams'(native, sid) {
    const started = Date.now()
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', 'echo ready; sleep 60 & sleep 60'] })
    await new Promise((resolve) => child.stdout.once('data', resolve))
    assert.equal(child.kill(), true)
    child.stdout.resume()
    child.stderr.resume()
    const outcome = await child.result.then((result) => result, (error) => error)
    if (!(outcome instanceof Error)) assert.notEqual(outcome.exit_code, 0)
    await Promise.all([closed(child.stdout), closed(child.stderr)])
    assert.ok(Date.now() - started < 10000, 'kill() did not end the command promptly')
  },

  async 'destroying stdout early stops a command that keeps writing'(native, sid) {
    const started = Date.now()
    const child = spawn(native, sid, { argv: ['/bin/sh', '-c', 'yes'] })
    await new Promise((resolve) => child.stdout.once('data', resolve))
    child.stdout.destroy()
    child.stderr.resume()
    const result = await child.result
    assert.notEqual(result.exit_code, 0, 'yes should die of EPIPE/SIGPIPE')
    assert.ok(Date.now() - started < 10000, 'the writer kept going after its reader closed')
  },

  async 'a command that cannot start rejects and still ends its streams'(native) {
    const child = spawn(native, 'no-such-session', { argv: ['/bin/true'] })
    const errors = []
    child.on('error', (error) => errors.push(error))
    await assert.rejects(child.result)
    await Promise.all([closed(child.stdout), closed(child.stderr)])
    assert.equal(errors.length, 1)
  },

  async 'the wrapper closes each Bun descriptor exactly once'(native, sid) {
    if (typeof Bun === 'undefined') return // Node's net.Socket owns its fd.
    // Descriptor numbers are reused, so track ownership over time: each fd
    // handed to the wrapper must be closed exactly once before its number is
    // handed out again, and closing a number not currently handed out would
    // be a double close.
    const open = new Set()
    let handedOut = 0
    let closedCount = 0
    const doubleCloses = []
    const tracking = {
      spawn: (...args) => {
        const child = native.spawn(...args)
        return new Proxy(child, {
          get(target, key) {
            const value = Reflect.get(target, key)
            if (key === 'takeStdoutFd' || key === 'takeStderrFd') {
              return () => {
                const fd = value.call(target)
                if (fd !== null) {
                  assert.ok(!open.has(fd), `fd ${fd} handed out while still open`)
                  open.add(fd)
                  handedOut += 1
                }
                return fd
              }
            }
            return typeof value === 'function' ? value.bind(target) : value
          },
        })
      },
    }
    const realClose = fs.closeSync
    fs.closeSync = (fd) => {
      if (open.has(fd)) {
        open.delete(fd)
        closedCount += 1
      } else if (handedOut > 0) {
        doubleCloses.push(fd)
      }
      return realClose(fd)
    }
    try {
      // Normal EOF.
      const eof = spawn(tracking, sid, { argv: ['/bin/sh', '-c', 'echo done'] })
      await Promise.all([collect(eof.stdout), collect(eof.stderr), eof.result])
      // Early destroy() while the command still writes.
      const destroyed = spawn(tracking, sid, { argv: ['/bin/sh', '-c', 'yes'] })
      await new Promise((resolve) => destroyed.stdout.once('data', resolve))
      destroyed.stdout.destroy()
      destroyed.stdout.destroy() // a second destroy must not close twice
      destroyed.stderr.resume()
      await destroyed.result
      // An error on the stream.
      const failed = spawn(tracking, sid, { argv: ['/bin/sh', '-c', 'sleep 0.2; echo late'] })
      failed.stdout.on('error', () => {})
      failed.stdout.destroy(new Error('consumer failed'))
      failed.stderr.resume()
      await failed.result
      // kill() mid-command.
      const killed = spawn(tracking, sid, { argv: ['/bin/sh', '-c', 'echo ready; sleep 60'] })
      await new Promise((resolve) => killed.stdout.once('data', resolve))
      killed.kill()
      killed.kill()
      killed.stdout.resume()
      killed.stderr.resume()
      await killed.result.catch(() => {})
      const streams = [eof, destroyed, failed, killed].flatMap((child) => [child.stdout, child.stderr])
      await Promise.all(streams.map(closed))
      await sleep(100)
      assert.equal(handedOut, 8)
      assert.equal(closedCount, 8, `${8 - closedCount} descriptors never closed`)
      assert.deepEqual([...open], [], 'descriptors left open')
      assert.deepEqual(doubleCloses, [], 'descriptors closed twice')
    } finally {
      fs.closeSync = realClose
    }
  },

  async 'exec takes an options object and an AbortSignal'(native, sid) {
    const result = await exec(native, sid, { argv: ['/bin/sh', '-c', 'printf "%s" "$GREETING"'], env: { GREETING: 'hi' } })
    assert.equal(result.stdout, 'hi')
    const controller = new AbortController()
    const started = Date.now()
    const running = exec(native, sid, { argv: ['/bin/sh', '-c', 'sleep 60'], signal: controller.signal })
    await sleep(300)
    controller.abort()
    const outcome = await running.then((result) => result, (error) => error)
    if (!(outcome instanceof Error)) assert.notEqual(outcome.exit_code, 0)
    assert.ok(Date.now() - started < 10000, 'abort did not end the command')
  },
}

async function main() {
  const [addon, helper, ...isolations] = process.argv.slice(2)
  if (!addon || !helper) throw new Error('usage: child.cjs <addon.node> <deltabox> [none|deltabox ...]')
  const { NativeDeltabox } = require(addon)
  for (const isolation of isolations.length ? isolations : ['none']) {
    await withSession(NativeDeltabox, helper, isolation, async (native, sid) => {
      // The first pass also warms up the runtime: Bun, for one, opens its
      // event-loop descriptors lazily. A real leak grows with every pass, so
      // the second pass must end with exactly the descriptors it started with.
      let before = null
      for (const pass of [1, 2]) {
        if (pass === 2) before = fdTargets()
        for (const [name, run] of Object.entries(cases)) {
          await run(native, sid, isolation)
          if (pass === 1) console.log(`ok   ${runtime} ${isolation}: ${name}`)
        }
      }
      await sleep(200)
      const after = fdTargets()
      if (before !== null) {
        const added = Object.entries(after).filter(([fd, target]) => before[fd] !== target && !target.startsWith('/proc'))
        assert.deepEqual(added, [], 'descriptors leaked by a full pass')
      }
      console.log(`ok   ${runtime} ${isolation}: a second full pass leaked no descriptor`)
    })
  }
}

main().catch((error) => {
  console.error(`FAIL ${runtime}:`, error)
  process.exitCode = 1
})
