"""Shared runners and measurement helpers for the exp2/exp3 benchmarks (Linux).

Every runner exposes the same lifecycle:

    start() -> ms                    open a session / transaction
    exec(argv) -> ms                 run one command inside it
    finalize(outcome) -> ms          "commit" or "abort"
    stop()                           tear down (never timed)
    io_bytes() -> int                cumulative bytes this system caused to be
                                     written to storage (see below)
    cow_bytes() -> int               current allocated size of the system's
                                     undo / copy-on-write storage

Disk-write accounting uses the kernel's per-task I/O counters
(`write_bytes`: bytes the task caused to be sent to the storage layer, charged
when a page is dirtied, so it does not depend on when writeback happens):

* processes the driver spawns and waits for (CLI calls, `sh`, `dd`, the raw
  Sandlock runner and its sandboxed children, `git`, `bwrap`) via
  `getrusage(RUSAGE_CHILDREN).ru_oublock * 512`;
* deltabox's long-lived helpers (fs-helper, command-helper, worker, plus the
  commands the worker reaps) via `/proc/<pid>/io`, found by session id.

The counters do not subtract `cancelled_write_bytes` (dirty pages of files
truncated or deleted before writeback), so they are an upper bound on what
reaches the device.
"""

import json
import os
import resource
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
DELTABOX = REPO / "target/release/deltabox"
RAW_SANDLOCK = REPO / "target/release/examples/raw_sandlock"
MiB = 1024 * 1024
KiB = 1024


def now():
    return time.perf_counter_ns()


def ms(t0, t1):
    return (t1 - t0) / 1e6


def run(argv, check=True, env=None, cwd=None):
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=cwd)
    if check and p.returncode != 0:
        raise RuntimeError(f"command failed ({p.returncode}): {argv}\n"
                           f"{p.stdout.decode(errors='replace')[-2000:]}\n"
                           f"{p.stderr.decode(errors='replace')[-2000:]}")
    return p


def timed(argv, **kw):
    t0 = now()
    p = run(argv, **kw)
    return ms(t0, now()), p


def allocated_bytes(path):
    """Allocated (not apparent) size of a file or tree, like `du -B1`."""
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        return 0
    total = st.st_blocks * 512
    if os.path.isdir(path) and not os.path.islink(path):
        try:
            with os.scandir(path) as it:
                for e in it:
                    total += allocated_bytes(e.path)
        except FileNotFoundError:
            pass
    return total


def children_write_bytes():
    return resource.getrusage(resource.RUSAGE_CHILDREN).ru_oublock * 512


def pids_with(token):
    token = token.encode()
    out = []
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/cmdline", "rb") as f:
                if token in f.read():
                    out.append(int(d))
        except OSError:
            pass
    return out


def proc_write_bytes(pid):
    try:
        with open(f"/proc/{pid}/io") as f:
            for line in f:
                if line.startswith("write_bytes:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def make_random_file(path, size, seed):
    """Deterministic pseudo-random, non-sparse content."""
    import random
    rng = random.Random(seed)
    chunk = MiB
    with open(path, "wb") as f:
        left = size
        while left:
            n = min(chunk, left)
            f.write(rng.randbytes(n))
            left -= n


def copy_file(src, dst):
    # ext4 has no reflink: this is a real data copy, so the copy is non-sparse
    # and independent of the source.
    shutil.copyfile(src, dst)


def dd_write(payload, target, write_size, offset, fsync=False):
    bs = min(write_size, MiB)
    conv = "notrunc,fsync" if fsync else "notrunc"
    return ["dd", f"if={payload}", f"of={target}", f"bs={bs}", f"count={write_size // bs}",
            f"seek={offset}", "oflag=seek_bytes", "iflag=fullblock", f"conv={conv}",
            "status=none"]


# ---------------------------------------------------------------- runners


class Bare:
    system, mode = "bare", "host"

    def __init__(self, project, fsync=False):
        self.project, self.fsync = Path(project), fsync

    def start(self):
        return 0.0

    def exec(self, argv):
        t, _ = timed(argv, cwd=self.project)
        return t

    def finalize(self, outcome):
        if outcome != "commit":
            raise ValueError("bare host execution cannot abort")
        return 0.0

    def stop(self):
        pass

    def io_bytes(self):
        return children_write_bytes()

    def cow_bytes(self):
        return 0


class Deltabox:
    system = "deltabox"

    def __init__(self, project, mode, durable, state_dir, deltabox=DELTABOX):
        self.project, self.mode, self.durable = Path(project), mode, durable
        self.bin = str(deltabox)
        self.state_dir = Path(state_dir)
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.env = {**os.environ, "DELTABOX_STATE_DIR": str(self.state_dir)}
        self.sid = None
        self._io_base = {}

    def _cli(self, *args):
        return timed([self.bin, *args], env=self.env)

    def start(self):
        args = ["session", "start", str(self.project), "--fs-mode", self.mode,
                "--require-fs-mode"] + (["--durable"] if self.durable else [])
        t, p = self._cli(*args)
        self.sid = p.stdout.decode().strip().splitlines()[-1]
        self._io_base = {}
        return t

    def exec(self, argv):
        t, p = self._cli("exec", self.sid, "--project", str(self.project), "--", *argv)
        res = json.loads(p.stdout)
        if res["exit_code"] != 0:
            raise RuntimeError(f"sandboxed command failed: {argv}\n{res}")
        return t

    def finalize(self, outcome):
        t, _ = self._cli(outcome, self.sid, "--project", str(self.project))
        return t

    def stop(self):
        if self.sid:
            run([self.bin, "session", "stop", self.sid, "--project", str(self.project)],
                check=False, env=self.env)
            # Stopped sessions keep their engine state and (cbcow-private) a
            # full materialized copy of the project; remove both so repeated
            # sessions do not fill the disk.
            for d in self._session_dirs():
                shutil.rmtree(d, ignore_errors=True)
            self.sid = None

    def _session_dirs(self):
        # With DELTABOX_STATE_DIR set, engine state lives in <state>/<sid> and
        # the per-project records (including cbcow-private's materialized
        # tree) in <state>/projects/<project-hash>/<sid>.
        return [self.state_dir / self.sid,
                *(self.state_dir / "projects").glob(f"*/{self.sid}")]

    def io_bytes(self):
        total = children_write_bytes()
        for pid in pids_with(self.sid):
            wb = proc_write_bytes(pid)
            if wb is None:
                continue
            # A helper that appears mid-interval started from zero.
            self._io_base.setdefault(pid, 0)
            total += wb
        return total

    def cow_bytes(self):
        return sum(allocated_bytes(d) for d in self._session_dirs())


class RawSandlock:
    """One fresh sandlock-core COW sandbox per run; see raw_sandlock.rs."""
    system, mode = "sandlock", "file-copy-up"

    def __init__(self, project, storage, runner=RAW_SANDLOCK):
        self.project, self.storage, self.bin = Path(project), Path(storage), str(runner)
        self.stats = self.storage.parent / f"{self.storage.name}.stats.json"
        self.last_upper = 0

    def run_once(self, argv, outcome):
        """Returns (write_ms, finalize_ms, upper_bytes) for one sandboxed run
        that ends in commit or abort. write_ms covers process start, sandbox
        setup, copy-up and the command; finalize_ms the commit/abort."""
        cmd = [self.bin] + (["--abort"] if outcome == "abort" else []) + [
            "--stats", str(self.stats), str(self.project), str(self.storage), "--", *argv]
        t, _ = timed(cmd)
        s = json.loads(self.stats.read_text())
        self.last_upper = s["upper_bytes"]
        return t - s["finalize_ms"], s["finalize_ms"], s["upper_bytes"]

    def io_bytes(self):
        return children_write_bytes()


class GitBwrap:
    """Git snapshot + Bubblewrap confinement baseline.

    Commands run directly on the host tree inside `bwrap` (read-only root,
    project bound read-write, private pid/net namespaces). A session snapshots
    the tree with `git add -A && git commit`; commit accepts the result with
    another commit; abort restores it with `git reset --hard && git clean -fd`.
    Git's own fsync defaults apply.
    """
    system, mode = "git-bwrap", "git-snapshot"

    def __init__(self, project):
        self.project = Path(project)
        self.git = ["git", "-C", str(self.project), "-c", "user.name=bench",
                    "-c", "user.email=bench@localhost", "-c", "commit.gpgsign=false"]
        if not (self.project / ".git").exists():
            run(self.git + ["init", "-q"])
            run(self.git + ["add", "-A"])
            run(self.git + ["commit", "-q", "-m", "baseline"])

    def _git(self, *cmds):
        t0 = now()
        for c in cmds:
            run(self.git + list(c))
        return ms(t0, now())

    def start(self):
        return self._git(["add", "-A"], ["commit", "-q", "--allow-empty", "-m", "session"])

    def exec(self, argv):
        p = str(self.project)
        t, _ = timed(["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                      "--bind", p, p, "--chdir", p, "--unshare-pid", "--unshare-net",
                      "--die-with-parent", *argv])
        return t

    def finalize(self, outcome):
        if outcome == "commit":
            return self._git(["add", "-A"], ["commit", "-q", "--allow-empty", "-m", "accept"])
        return self._git(["reset", "-q", "--hard"], ["clean", "-fdq"])

    def stop(self):
        pass

    def io_bytes(self):
        return children_write_bytes()

    def cow_bytes(self):
        return allocated_bytes(self.project / ".git")


def env_info(workdir):
    fs = run(["findmnt", "-no", "FSTYPE", "-T", str(workdir)]).stdout.decode().strip()
    cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
                if l.startswith("model name")), "")
    rev = run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"], check=False
              ).stdout.decode().strip()
    dirty = run(["git", "-C", str(REPO), "status", "--porcelain", "--untracked-files=no"],
                check=False).stdout.decode().strip()
    ver = run([str(DELTABOX), "--version"], check=False).stdout.decode().strip()
    import platform
    return {"kernel": platform.release(), "cpu": cpu, "cpus": os.cpu_count(),
            # Repo-relative, so results carry no home directory or user name.
            "workdir": (os.path.relpath(workdir, REPO) if Path(workdir).is_relative_to(REPO)
                        else Path(workdir).name),
            "workdir_fs": fs, "git": rev + ("-dirty" if dirty else ""),
            "deltabox": ver, "python": sys.version.split()[0]}


def reflink_supported(workdir):
    a, b = Path(workdir) / ".reflink-probe-a", Path(workdir) / ".reflink-probe-b"
    a.write_bytes(b"x" * 4096)
    try:
        ok = subprocess.run(["cp", "--reflink=always", str(a), str(b)],
                            stderr=subprocess.DEVNULL).returncode == 0
    finally:
        a.unlink(missing_ok=True)
        b.unlink(missing_ok=True)
    return ok
