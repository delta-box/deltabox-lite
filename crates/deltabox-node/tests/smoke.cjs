#!/usr/bin/env node

const assert = require('node:assert/strict')
const { mkdtemp, readFile, realpath, rm, stat } = require('node:fs/promises')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

async function main() {
  const [addonPath, helperBinary, requestedIsolation, requestedMode] = process.argv.slice(2)
  if (!addonPath || !helperBinary) {
    throw new Error('usage: smoke.cjs <addon.node> <deltabox> [deltabox|bubblewrap|docker|none] [cbcow-through|git]')
  }
  const mode = requestedMode || 'cbcow-through'
  if (mode !== 'cbcow-through' && mode !== 'git') {
    throw new Error(`unsupported smoke mode ${mode}`)
  }
  const gitMode = mode === 'git'
  const isolation = requestedIsolation || (gitMode ? 'bubblewrap' : 'none')
  if (!['deltabox', 'bubblewrap', 'docker', 'none'].includes(isolation)) {
    throw new Error(`unsupported smoke isolation ${isolation}`)
  }
  // A git session is served by its own namespace or container sandbox; the
  // CB-CoW family runs in the session's Deltabox sandbox or the ambient
  // environment. The two choices are locked together in both directions.
  const gitIsolation = isolation === 'bubblewrap' || isolation === 'docker'
  if (gitMode !== gitIsolation) {
    throw new Error(`git storage needs bubblewrap or docker isolation: got ${mode}+${isolation}`)
  }
  const { NativeDeltabox } = require(addonPath)
  const project = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-smoke-')))
  const state = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-state-')))
  const runtime = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-runtime-')))
  const file = join(project, 'native-buffer.txt')
  process.env.DELTABOX_NATIVE_SHARED_ENV = 'shared-from-node'
  const native = new NativeDeltabox(project, helperBinary, {
    stateDir: state,
    runtimeDir: runtime,
    fsMode: mode,
    isolation,
  })
  let session
  try {
    // A git session is served by its own bubblewrap or container sandbox and
    // has no mount of its own, so it is the one mode that has to name its
    // storage explicitly instead of letting the coordinator auto-resolve it.
    if (gitMode) {
      session = await native.startSession(undefined, 'fast')
      assert.equal(session.mountpoint === undefined, true)
    } else {
      session = await native.startSession(undefined, 'fast', undefined, 'auto', false)
      assert.ok(session.mountpoint)
    }
    assert.equal(session.resolved_fs_mode, mode)
    assert.equal(session.durability, 'process-crash')
    assert.equal(session.status, 'running')
    assert.equal(session.isolation, isolation)
    assert.equal(session.isolation_provider === null, isolation === 'none')
    if (gitMode) {
      assert.equal(session.rollback_guarantee, 'git-tracked-files')
    } else {
      assert.equal(session.state_provider, 'macos-apfs-fsevents')
      assert.equal(session.fallback_reason, null)
    }

    const baseline = await native.checkpoint(session.session_id, 'baseline')
    assert.equal(baseline.label, 'baseline')

    // The container command also proves the workspace is writable at the mount
    // point and that the image filesystem stays read-only.
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
    // A git session runs in bubblewrap or a container, neither of which
    // inherits the Node process ambient environment the way `none` does.
    assert.equal(exec.stdout, isolation === 'docker'
      ? ':explicit:/workspace'
      : gitMode ? ':explicit' : 'shared-from-node:explicit')
    if (isolation === 'docker') {
      const commandFile = join(project, 'docker-command.txt')
      assert.equal(await readFile(commandFile, 'utf8'), 'container\n')
      assert.equal((await stat(commandFile)).uid, process.getuid())
    }

    const review = await native.beginReview(session.session_id, 'task', 'native smoke')
    assert.equal((await native.reviewStatus(session.session_id)).kind, 'task')

    const content = Buffer.from([0, 1, 2, 253, 254, 255])
    await native.writeFile(session.session_id, file, content)
    assert.deepEqual(await native.readFile(session.session_id, file), content)
    assert.deepEqual(await readFile(file), content)
    const diff = await native.reviewDiff(session.session_id)
    assert.equal(diff.pending_changes, 1)
    // The container command added a file before the review opened, so only the
    // diff against the pre-exec baseline sees it.
    assert.equal(
      (await native.diff(session.session_id, baseline.id)).pending_changes,
      isolation === 'docker' ? 2 : 1,
    )
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

    // A per-command execution profile widens host access for one call without
    // relaxing the session: the workspace stays DeltaFS-managed and the backing
    // project stays denied.
    if (isolation === 'deltabox') {
      const outside = await realpath(await mkdtemp(join(tmpdir(), 'deltabox-node-outside-')))
      const outsideFile = join(outside, 'outside.txt')
      const denied = await native.exec(
        session.session_id,
        ['/bin/sh', '-c', `printf nope > '${outsideFile}'`],
        project,
        undefined,
        'workspace-write profile',
      )
      assert.notEqual(denied.exit_code, 0)
      const widened = await native.exec(
        session.session_id,
        ['/bin/sh', '-c', `printf widened > '${outsideFile}'`],
        project,
        undefined,
        'read-write profile',
        'read-write',
      )
      assert.equal(widened.exit_code, 0)
      assert.equal(await readFile(outsideFile, 'utf8'), 'widened')
      const leak = join(project, 'native-direct.txt')
      const bypass = await native.exec(
        session.session_id,
        ['/bin/sh', '-c', `printf hacked > '${leak}'`],
        project,
        undefined,
        'backing-project probe',
        'read-write',
      )
      assert.notEqual(bypass.exit_code, 0)
      await rm(outside, { recursive: true, force: true })
    }

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
