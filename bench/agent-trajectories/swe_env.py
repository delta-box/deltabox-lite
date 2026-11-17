"""SWE-bench task environments without Docker.

Each SWE-bench task has an official image on Docker Hub
(`swebench/sweb.eval.x86_64.<owner>_1776_<repo>-<n>`). This module pulls the
amd64 image anonymously from the registry, unpacks its layers into a plain
directory (applying OCI whiteouts), and runs commands in it with unprivileged
bubblewrap, the way mini-SWE-agent runs them with `docker exec`:

    docker exec -w /testbed -e <agent env> <container> bash -c <command>

(the interpreter, cwd, env and 60 s timeout are recorded in each trajectory's
info.config.environment).

Inside bwrap the image root is `/`, the user is mapped to uid 0 (as in the
container), the host network and DNS are shared, and `/tmp` is the image's own
(persistent across commands, as in a container).
"""

import json
import os
import shutil
import subprocess
import tarfile
import time
import urllib.error
import urllib.request
from pathlib import Path

REGISTRY = "https://registry-1.docker.io"
MARGIN = 1 << 30  # always leave 1 GiB free
# mini-swe-agent's SWE-bench config sets these for every command.
AGENT_ENV = {"PAGER": "cat", "MANPAGER": "cat", "LESS": "-R", "PIP_PROGRESS_BAR": "off",
             "TQDM_DISABLE": "1"}


def image_name(instance_id):
    owner, rest = instance_id.split("__", 1)
    return f"swebench/sweb.eval.x86_64.{owner}_1776_{rest}".lower()


def _get(url, token, accept=None):
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}",
                                               **({"Accept": accept} if accept else {})})
    return urllib.request.urlopen(req, timeout=120)


def _token(repo):
    url = ("https://auth.docker.io/token?service=registry.docker.io"
           f"&scope=repository:{repo}:pull")
    return json.load(urllib.request.urlopen(url, timeout=60))["token"]


def resolve(repo, tag="latest"):
    """Returns (manifest_digest, manifest, config) of the linux/amd64 image."""
    tok = _token(repo)
    idx = json.load(_get(f"{REGISTRY}/v2/{repo}/manifests/{tag}", tok,
                         "application/vnd.oci.image.index.v1+json, "
                         "application/vnd.docker.distribution.manifest.list.v2+json"))
    digest = next(m["digest"] for m in idx["manifests"]
                  if m["platform"].get("architecture") == "amd64"
                  and m["platform"].get("os") == "linux")
    man = json.load(_get(f"{REGISTRY}/v2/{repo}/manifests/{digest}", tok,
                         "application/vnd.oci.image.manifest.v1+json, "
                         "application/vnd.docker.distribution.manifest.v2+json"))
    cfg = json.load(_get(f"{REGISTRY}/v2/{repo}/blobs/{man['config']['digest']}", tok))
    return digest, man, cfg


def _rm(p):
    if p.is_symlink() or p.is_file():
        p.unlink()
    elif p.is_dir():
        shutil.rmtree(p)


def unpack(repo, root, blobs, log=print):
    """Pull `repo` and unpack a fresh copy into `root` (layer blobs are kept in
    `blobs` so a second copy needs no download). Returns provenance metadata."""
    root, blobs = Path(root), Path(blobs)
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    blobs.mkdir(parents=True, exist_ok=True)
    mf = blobs / "image.json"
    if mf.exists():
        digest, man, cfg = json.loads(mf.read_text())
    else:
        digest, man, cfg = resolve(repo)
    # Disk guard: unpacked images are ~2.5x their compressed layers; keeping
    # the layers for a second unpack needs 1x more. Refuse rather than fill the disk.
    size = sum(l["size"] for l in man["layers"])
    cached = sum(l["size"] for l in man["layers"]
                 if (blobs / l["digest"].replace(":", "_")).exists())
    free = shutil.disk_usage(root).free
    keep = free > 3 * size + 1.5 * (size - cached) + MARGIN
    need = 3 * size + max(l["size"] for l in man["layers"]) + MARGIN
    log(f"  image {size >> 20} MiB compressed, {free >> 20} MiB free"
        f"{'' if keep else ', not caching layers'}")
    if not keep and free < need:
        shutil.rmtree(root)
        raise RuntimeError(f"not enough disk for {repo}: {free >> 20} MiB free, "
                           f"~{need >> 20} MiB needed")

    for n, layer in enumerate(man["layers"]):
        tmp = blobs / layer["digest"].replace(":", "_")
        if shutil.disk_usage(root).free < MARGIN:
            raise RuntimeError("disk nearly full while unpacking")
        if not tmp.exists():
            log(f"  layer {n + 1}/{len(man['layers'])}: download {layer['size'] >> 20} MiB")
            part = tmp.with_suffix(".part")
            # Anonymous registry tokens expire after ~5 min: one per layer,
            # and one retry with a new token on 401.
            for attempt in (0, 1):
                try:
                    with _get(f"{REGISTRY}/v2/{repo}/blobs/{layer['digest']}",
                              _token(repo)) as r, open(part, "wb") as f:
                        shutil.copyfileobj(r, f, 1 << 20)
                    break
                except urllib.error.HTTPError as e:
                    if e.code != 401 or attempt:
                        raise
            part.rename(tmp)
        # Apply this layer's whiteouts to the lower layers, then extract the rest.
        with tarfile.open(tmp) as t:
            names = t.getnames()
        excl = []
        for name in names:
            p = Path(name)
            if p.name == ".wh..wh..opq":
                d = root / p.parent
                if d.is_dir():
                    for c in d.iterdir():
                        _rm(c)
                excl.append(name)
            elif p.name.startswith(".wh."):
                _rm(root / p.parent / p.name[4:])
                excl.append(name)
        excl_file = root.parent / f"{root.name}.excl"
        excl_file.write_text("\n".join(excl + ["dev/*", "./dev/*"]) + "\n")
        # GNU tar: no ownership change (unprivileged), keep modes, never
        # follow existing symlinks out of the tree (default behaviour), skip
        # device nodes.
        subprocess.run(["tar", "-xzf", str(tmp), "-C", str(root), "--no-same-owner",
                        "--delay-directory-restore", "-X", str(excl_file)],
                       stderr=subprocess.DEVNULL)
        excl_file.unlink()
        if not keep:
            tmp.unlink()
    mf.write_text(json.dumps([digest, man, cfg]))
    # Make everything owner-writable so the unprivileged user (uid 0 inside
    # bwrap) can modify it like root would.
    subprocess.run(["chmod", "-R", "u+rwX", str(root)], stderr=subprocess.DEVNULL)
    meta = {"image": repo, "manifest_digest": digest,
            "env": cfg.get("config", {}).get("Env", []),
            "workdir": cfg.get("config", {}).get("WorkingDir", ""),
            "unpacked": time.strftime("%F %T")}
    (root / ".unpacked.json").write_text(json.dumps(meta, indent=2))
    return meta


class Env:
    """Runs agent commands inside an unpacked image."""

    def __init__(self, root, meta, cwd="/testbed", timeout=60):
        self.root, self.meta, self.cwd, self.timeout = Path(root), meta, cwd, timeout
        self.env = dict(e.split("=", 1) for e in meta["env"])
        self.env.update(AGENT_ENV)
        self.env.setdefault("HOME", "/root")

    def host(self, path):
        """Host path of a path inside the environment."""
        return self.root / str(path).lstrip("/")

    def argv(self, command, wrap=()):
        a = ["bwrap", "--bind", str(self.root), "/", "--proc", "/proc", "--dev", "/dev",
             "--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf",
             "--unshare-user", "--uid", "0", "--gid", "0", "--unshare-pid",
             "--die-with-parent", "--chdir", self.cwd, "--clearenv"]
        for k, v in self.env.items():
            a += ["--setenv", k, v]
        return a + list(wrap) + ["bash", "-c", command]

    def run(self, command, wrap=()):
        t0 = time.perf_counter()
        try:
            p = subprocess.run(self.argv(command, wrap), stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, timeout=self.timeout)
            rc, out = p.returncode, p.stdout
        except subprocess.TimeoutExpired as e:
            rc, out = -9, e.stdout or b""
        return rc, out, (time.perf_counter() - t0) * 1e3
