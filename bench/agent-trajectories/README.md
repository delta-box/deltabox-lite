# Transactional agent trajectories

Ten real coding-agent trajectories from SWE-bench Verified are turned into a
sequence of **transactions**, each ending in **commit** or **abort**. The aborts
are not invented: they are places where the agent itself threw work away
(`cp X.backup X`, `git checkout X`, `git stash` … `pop`), rewritten as a
rollback to a checkpoint. Commit and abort are then pluggable, so the same
trajectory can be replayed with different implementations (the agent's own
commands, git, DeltaBox Lite, Sandlock, …) and the costs compared.

Status: 8 of the 10 trajectories have been replayed in their task images with
the `manual` and `git` backends: every exit code matches the recording and the
two backends leave identical workspaces at every transaction boundary
(`results/summary.per_eval.md`). The two xarray images (~2 GB compressed) have
not been replayed yet (not enough disk on the machine that built this).

## Where the trajectories come from

| | |
|---|---|
| Benchmark | [SWE-bench Verified](https://www.swebench.com/) (500 human-validated GitHub issues) |
| Leaderboard track | bash-only |
| Agent | [mini-SWE-agent](https://github.com/SWE-agent/mini-swe-agent) (MIT). Every action is one bash command run in `/testbed`, so a trajectory is an ordered list of shell commands |
| Version | run metadata says 2.0.0; the trajectory files record `mini_version` 2.1.0 |
| Model | `claude-sonnet-4-5-20250929`, reasoning effort high (71.4% resolved on the full run) |
| Run | [`20260217_mini-v2.0.0_claude-4-5-sonnet-high`](https://github.com/SWE-bench/experiments/tree/main/evaluation/verified/20260217_mini-v2.0.0_claude-4-5-sonnet-high) in [SWE-bench/experiments](https://github.com/SWE-bench/experiments) |
| Raw trajectories | `https://swe-bench-submissions.s3.amazonaws.com/bash-only/<run>/trajs/<id>/<id>.traj.json` (public) |
| Resolved / unresolved | the run's `per_instance_details.json` |
| Repo and base commit | SWE-bench Verified dataset (`princeton-nlp/SWE-bench_Verified` on Hugging Face) |
| Execution environment (as recorded) | image `swebench/sweb.eval.x86_64.<id>`, `cwd=/testbed`, `bash -c`, 60 s timeout, env `PAGER=cat MANPAGER=cat LESS=-R PIP_PROGRESS_BAR=off TQDM_DISABLE=1` |

**Selection.** From a 60-trajectory sample of the run (every 8th trajectory in
S3 key order), every trajectory whose commands match an explicit-revert
pattern, plus two without one as controls. `sources.json` lists them with the
SHA-256 of each raw file; `fetch.py` verifies the hashes.

| instance | repo | steps | revert events | aborts | commits / aborts (per_eval) | resolved | agent wall time (min) |
|---|---|---:|---:|---:|---:|---|---:|
| django__django-11433 | django/django | 58 | 2 | 1 | 26 / 1 | yes | 6.9 |
| django__django-12308 | django/django | 50 | 0 | 0 | 19 / 0 | no | 4.7 |
| django__django-14373 | django/django | 27 | 0 | 0 | 11 / 0 | yes | 3.4 |
| pydata__xarray-4075 | pydata/xarray | 38 | 2 | 1 | 19 / 1 | yes | 5.3 |
| pydata__xarray-6599 | pydata/xarray | 68 | 2 | 1 | 40 / 1 | no | 9.5 |
| pylint-dev__pylint-4551 | pylint-dev/pylint | 105 | 0 | 0 | 49 / 0 | no | 7.1 |
| scikit-learn__scikit-learn-11310 | scikit-learn/scikit-learn | 67 | 1 | 1 | 16 / 1 | yes | 5.6 |
| sphinx-doc__sphinx-8595 | sphinx-doc/sphinx | 69 | 1 | 1 | 18 / 1 | yes | 4.5 |
| sympy__sympy-13372 | sympy/sympy | 46 | 3 | 3 | 11 / 3 | yes | 3.5 |
| sympy__sympy-13877 | sympy/sympy | 66 | 2 | 0 | 25 / 0 | no | 7.0 |

The commit/abort rate is fixed per trajectory: it is whatever the agent did.

**Licensing.** mini-SWE-agent and SWE-bench are MIT. The trajectory archive
(S3) and SWE-bench/experiments carry no licence, so this folder does **not**
contain the transcripts or command text. It stores references (URL + SHA-256)
and derived annotations (per-step kind, written paths, transactions);
`fetch.py` + `annotate.py` rebuild the full replay files locally in
`target/agent-traj-cache/replay/`.

## How commit and abort are derived

`annotate.py` reads every command and models its effect on files under
`/testbed` (caches excluded: `.git`, `__pycache__`, `*.pyc`, `.pytest_cache`,
`.mypy_cache`, `.tox`, `*.egg-info`):

* **operations:** full write (`cat > F << EOF`, `> F`, `tee`; heredoc content is
  hashed so an identical rewrite is recognised), partial write (`sed -i`, `>>`,
  a Python script, `-c` code or heredoc that opens a literal path for writing),
  copy, move, delete (including `rm` globs), `git checkout`/`restore <file>`
  (back to the base version), `git stash` / `stash pop`, and uses (a command
  naming the path). Anything it cannot resolve (`patch`, `git apply`,
  `find -exec sed -i`, a script writing to a computed path) is an unknown write;
* **versions:** each path is at its base version (tracked at the task's base
  commit, from the GitHub tree), absent, or a content version.

A **revert event** is a command after which files are back at a version they
had earlier. Its **span** runs from the first command that moved them away to
the command before the revert. The event becomes an **abort** when rolling the
whole workspace back to a checkpoint taken just before the span has the same
effect as the agent's revert:

1. no command in the span has an unknown write;
2. it is not a temporary revert: the span's version is not brought back later
   (`git stash` … `git stash pop`, `cp X X.new && git checkout X` … `cp X.new X`);
   in that case the *experiment on the old code* is the aborted span instead;
3. every other path changed in the span is back to its pre-span version after
   the revert, or is overwritten or deleted before it is used again; a new
   untracked file that is never used again is allowed and listed as
   `dropped_unused_scratch`;
4. the revert command does nothing else (apart from creating an unused new
   file);
5. it does not overlap an earlier abort.

The accepted abort replaces the agent's revert command: the replay rolls back
and skips it. Everything else is commit. Each event's decision and reason is in
`revert_events`.

Examples (django__django-11433):

* steps 29–32: `cp models.py models.py.backup`, a patch script edits
  `models.py`, it is inspected, `cp models.py.backup models.py`. **Abort** of
  steps 30–31; step 32 is replaced by the rollback.
* steps 33–40: a second patch, two new test scripts, then the backup is
  restored again. **Commit**: rolling back would also delete
  `test_comprehensive.py`, which the agent runs at step 42. The agent's undo
  was per-file; a checkpoint rollback is whole-workspace.

Partial undos like the second one are common (sympy__sympy-13877 reverts four
files and keeps a fifth), which is why only 8 of the 13 revert events become
aborts.

## Commit policies

Where commits happen is a modelling choice; both are in every annotation:

| policy | commits | use |
|---|---|---|
| `minimal` | only where an abort needs a checkpoint before it, and at the end | fewest, largest transactions |
| `per_eval` | also after every evaluation command (a test or script run) outside an abort span: the agent checkpoints after each check | many small transactions, including empty ones (stress case: a harness that does not track changes) |
| `per_write` | like `per_eval`, but only if a file changed since the last commit | no empty commits (a harness that commits only when there is something to commit) |

## Files

| file | |
|---|---|
| `sources.json` | run, instances, selection rule, SHA-256 of every raw trajectory |
| `fetch.py` | downloads and verifies the raw trajectories into `target/agent-traj-cache/raw/` |
| `annotate.py` | builds `annotations/<id>.json` (committed, no command text) and `target/agent-traj-cache/replay/<id>.json` (local, with command text) |
| `annotations/<id>.json` | provenance, `steps` (kind, recorded exit code, written paths, command SHA-256), `transactions.{minimal,per_eval}`, `revert_events`, `session_outcome` (commit if resolved), `summary` |
| `replay.py` | the replay driver and backends; `--dry-run` checks every schedule |
| `replay_all.sh` | every trajectory × backend, one image at a time, then `summarize.py` |
| `summarize.py` | fidelity and cost tables → `results/summary.<policy>.md` |
| `results/<id>.<backend>.<policy>.csv` | one row per begin/exec/commit/abort: ms, exit code vs recorded, workspace hash (command output goes to `target/agent-traj-cache/outputs/`, not committed) |
| `report.py`, `report_template.html` | `target/agent-traj-cache/report.<policy>.html`: every step with its time per backend, commit/abort lines highlighted, policies compared (local only: it quotes the commands) |
| `swe_env.py` | pulls a task image from Docker Hub without Docker, unpacks it, runs commands in it with unprivileged bubblewrap as uid 0 |
| `exact.py` | `per_write` commit/abort from real replays: observe every workspace state twice, derive commits and aborts, verify them with git rollbacks → `results/exact/<id>.json` (no command text) |
| `pypi_asof.py` | local PyPI index frozen at the recording time, so the agents' `pip install` gets the packages it got then |
| `aggregate.py` | per-repository table from `results/exact/` → `results/exact-summary.{md,csv}` |
| `.github/workflows/agent-trajectories.yml` | `exact.py` over every trajectory of the run in 100 parallel jobs; the table is the run's summary, the JSON its artifact |

A transaction:

```json
{"id": 11, "first": 30, "last": 31, "outcome": "abort", "checkpoint_before": 30,
 "replaces": [32], "files": ["django/forms/models.py", "patch_construct_instance.py"],
 "writes": 2, "evals": 1}
```

## Reproduce

```sh
cd bench/agent-trajectories
python3 fetch.py               # download + verify (uses network)
python3 annotate.py            # needs `gh` for the repo trees
python3 replay.py --dry-run    # check every schedule
python3 replay.py django__django-14373 --backend git   # replay one in its task image
./replay_all.sh per_eval manual git                     # all of them, then summarize.py
```

Needs `bwrap` (unprivileged user namespaces) and network access to Docker Hub;
no Docker daemon.

## Pluggable backends

The driver walks one policy's transactions and calls
`setup`, `begin(tx)`, `exec(cmd)`, `commit(tx)`, `abort(tx)`, `teardown`.
Implemented: `manual` (as recorded: an abort runs the agent's own revert
command) and `git` (private `GIT_DIR` over `/testbed`; commit = `add -A` +
`commit`, abort = `reset --hard` + `clean -fd`).

### Adding a backend

| backend | setup | begin(tx) | exec | commit | abort |
|---|---|---|---|---|---|
| DeltaBox Lite | `session start /testbed --fs-mode <mode> [--durable]` | `checkpoint` | `deltabox exec <sid> -- bash -c <cmd>` | `commit` | `abort --checkpoint <id>` |
| Sandlock | – | start one sandbox running a shell loop | send the command over its pipe, wait for its exit code | end the sandbox with `on_exit=Commit` | end it with `on_exit=Abort` |
| reflink snapshot | – | `cp -a --reflink` of the tree | run in place | drop the snapshot | restore the snapshot |

Not implemented yet: DeltaBox and Sandlock have to run inside the task image
(its Python, `/testbed`), which needs the binaries built for the image's libc
and FUSE inside the bubblewrap environment.

## Status and limitations

* **Validation.** The labels come from static analysis; the replay checks
  them. Each run records every command's exit code (compared with the
  recording) and a hash of `/testbed` after every commit/abort (compared with
  the `manual` run, i.e. the agent's own revert commands). Files the rollback
  drops by design are left out of the comparison: `dropped_unused_scratch`
  (e.g. a `.orig` the revert command creates) and
  `differs_until_overwritten` (a dead file from the aborted span, until the
  step that deletes or rewrites it). `./replay_all.sh` runs everything, one
  image on disk at a time (~1 GB layers + ~3 GB unpacked; xarray ~2× that),
  and refuses an image that would leave less than 1 GiB free.
* Only `/testbed` is transactional. Files the agent keeps in `/tmp` or
  installs with `pip` are outside the workspace, as they are outside a
  sandbox's project.
* A file seen only through a directory scan (`grep -r`, `ls`, `git status`) is
  not counted as a use, so a rollback can change such a command's output but
  not the workspace.
* Build artefacts (`setup.py build_ext --inplace`) are not modelled.
* Replay is open-loop: commands come from the recording; outputs may differ
  where the environment differs (exit codes are kept as `recorded_rc` to
  compare).
