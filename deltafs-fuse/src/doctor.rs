use serde::Serialize;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub os: &'static str,
    pub arch: &'static str,
    pub fuse_feature: bool,
    pub dev_fuse: bool,
    pub fusermount: Option<PathBuf>,
    pub macfuse_installed: bool,
    pub macfuse_pkg_config: bool,
    pub native_provider_ready: bool,
    pub mount_ready: bool,
    pub notes: Vec<String>,
}

pub fn inspect() -> DoctorReport {
    let os = env::consts::OS;
    let dev_fuse = Path::new("/dev/fuse").exists();
    let fusermount = find_command("fusermount3").or_else(|| find_command("fusermount"));
    let macfuse_installed = Path::new("/Library/Filesystems/macfuse.fs").exists();
    let macfuse_pkg_config = Command::new("pkg-config")
        .args(["--exists", "fuse"])
        .status()
        .is_ok_and(|status| status.success());
    let fuse_feature = cfg!(feature = "fuse") && !cfg!(feature = "macos-no-mount");
    let native_provider_ready = os == "macos" && Path::new("/usr/bin/sandbox-exec").is_file();

    let mount_ready = match os {
        "linux" => fuse_feature && dev_fuse && fusermount.is_some(),
        "macos" => fuse_feature && macfuse_installed && macfuse_pkg_config,
        _ => false,
    };

    let mut notes = Vec::new();
    match os {
        "linux" => {
            if !dev_fuse {
                notes.push("/dev/fuse is unavailable".into());
            }
            if fusermount.is_none() {
                notes.push("fusermount3/fusermount is unavailable".into());
            }
        }
        "macos" => {
            if native_provider_ready {
                notes.push(
                    "native APFS/FSEvents sessions are available without a FUSE runtime".into(),
                );
            } else {
                notes.push("/usr/bin/sandbox-exec is unavailable".into());
            }
            if !mount_ready {
                notes.push("the optional macFUSE mount adapter is not ready".into());
            }
        }
        _ => notes.push("deltafs-fuse supports Linux and macOS only".into()),
    }
    if !fuse_feature {
        notes.push("binary was built without an active FUSE mount feature".into());
    }

    DoctorReport {
        os,
        arch: env::consts::ARCH,
        fuse_feature,
        dev_fuse,
        fusermount,
        macfuse_installed,
        macfuse_pkg_config,
        native_provider_ready,
        mount_ready,
        notes,
    }
}

fn find_command(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
