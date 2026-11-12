// Host disposal preserves external edits; explicit stop retains rollback semantics.
const assert = require('node:assert/strict')
const fs = require('node:fs/promises')
const { join } = require('node:path')
const { tmpdir } = require('node:os')
const [addon, helper] = process.argv.slice(2)
const { NativeDeltabox } = require(addon)
;(async () => {
  for (const isolation of ['none', 'deltabox']) for (const durability of ['fast', 'strict']) {
    const root = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'deltabox-release-')))
    const project = join(root, 'project')
    await fs.mkdir(project)
    await fs.writeFile(join(project, 'a'), 'baseline')
    await fs.writeFile(join(project, 'deleted'), 'remove externally')
    const create = () => new NativeDeltabox(project, helper, { stateDir: join(root, 'state'), runtimeDir: join(root, 'runtime'), fsMode: 'cbcow-through', isolation })
    let native = create(), sid
    try {
      sid = (await native.startSession(undefined, durability)).session_id
      await native.beginReview(sid, 'task', 'unresolved review')
      await native.writeFile(sid, join(project, 'a'), Buffer.from('review draft'))
      await assert.rejects(native.releaseSession(sid), /no active review/)
      assert.equal(await fs.readFile(join(project, 'a'), 'utf8'), 'review draft')
      await native.reviewAbort(sid)
      await fs.writeFile(join(project, 'a'), 'external')
      await fs.unlink(join(project, 'deleted'))
      await fs.mkdir(join(project, '.git'))
      await fs.writeFile(join(project, '.git', 'HEAD'), 'external ref')
      assert.equal((await native.releaseSession(sid)).status, 'stopped')
      sid = undefined
      assert.equal(await fs.readFile(join(project, 'a'), 'utf8'), 'external')
      assert.equal(await fs.readFile(join(project, '.git', 'HEAD'), 'utf8'), 'external ref')
      await assert.rejects(fs.stat(join(project, 'deleted')), { code: 'ENOENT' })
      native = create()
      sid = (await native.startSession(undefined, durability)).session_id
      await native.writeFile(sid, join(project, 'a'), Buffer.from('pending explicit stop'))
      await native.stopSession(sid); sid = undefined
      assert.equal(await fs.readFile(join(project, 'a'), 'utf8'), 'external')
      console.log(JSON.stringify({ isolation, durability, releasePreservedExternalEdits: true, unresolvedReviewRefused: true, explicitStopStillAborts: true, restart: true }))
    } finally {
      if (sid) await native.stopSession(sid)
      await fs.rm(root, { recursive: true, force: true })
    }
  }
})().catch(error => { console.error(error); process.exitCode = 1 })
