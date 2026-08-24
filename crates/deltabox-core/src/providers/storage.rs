//! Git-backed ghost storage: every snapshot is a commit in a private bare
//! repository. The project root never needs to be a git repository and its
//! `.git`, index, HEAD, and branches are never read or written: every command
//! runs with `--git-dir` on the side repo, `--work-tree` on the project root,
//! and a per-operation temporary `GIT_INDEX_FILE`.

use crate::changeset::{Change, ChangeKind};
use crate::error::{Error, Result};
use crate::protocol::DiffResult;
use crate::providers::{canonicalize_allow_missing, StorageProvider};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Namespace holding every ghost commit inside the side repo.
pub const GHOST_REF_PREFIX: &str = "refs/deltabox/undo/";

/// Per-file text-diff byte budget; larger or binary files carry no text diff.
pub const GHOST_DIFF_BASIS_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Binary-content probe reads at most this many bytes.
const BINARY_SAMPLE_BYTES: usize = 8192;

/// Text-diff generation chunk so one git patch process stays memory-bounded.
const TEXT_PATCH_CHUNK: usize = 8;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

const GIT_IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "deltabox"),
    ("GIT_AUTHOR_EMAIL", "undo@deltabox.invalid"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00"),
    ("GIT_COMMITTER_NAME", "deltabox"),
    ("GIT_COMMITTER_EMAIL", "undo@deltabox.invalid"),
    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00"),
];

struct NameStatusEntry {
    kind: char,
    path: String,
}

struct Numstat {
    added: u64,
    removed: u64,
    binary: bool,
}

fn contains_path(parent: &Path, child: &Path) -> bool {
    child.strip_prefix(parent).is_ok()
}

fn parse_name_status(output: &str) -> Result<Vec<NameStatusEntry>> {
    let parts: Vec<&str> = output.split('\0').collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index + 1 < parts.len() {
        let status = parts[index];
        let path = parts[index + 1];
        index += 2;
        if status.is_empty() || path.is_empty() {
            continue;
        }
        if !matches!(status, "A" | "M" | "D" | "T") {
            return Err(Error::Backend(format!(
                "git.ghost: unexpected diff status {status:?} for {path:?}"
            )));
        }
        entries.push(NameStatusEntry {
            kind: status.chars().next().unwrap(),
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn parse_numstat(output: &str) -> HashMap<String, Numstat> {
    let mut by_path = HashMap::new();
    for record in output.split('\0') {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(removed)) = (parts.next(), parts.next()) else {
            continue;
        };
        let path = parts.next().unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let binary = added == "-" || removed == "-";
        by_path.insert(
            path.to_string(),
            Numstat {
                added: if binary {
                    0
                } else {
                    added.parse().unwrap_or(0)
                },
                removed: if binary {
                    0
                } else {
                    removed.parse().unwrap_or(0)
                },
                binary,
            },
        );
    }
    by_path
}

/// `git ls-tree -z -r` records: `<mode> <type> <hash>\t<path>\0`; keep blobs.
fn parse_tree_list(output: &str) -> HashMap<String, String> {
    let mut by_path = HashMap::new();
    for record in output.split('\0') {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.find('\t') else {
            continue;
        };
        let meta: Vec<&str> = record[..tab].split(' ').collect();
        if meta.get(1) != Some(&"blob") || meta.get(2).is_none() {
            continue;
        }
        by_path.insert(record[tab + 1..].to_string(), meta[2].to_string());
    }
    by_path
}

/// One `cat-file --batch-check` round trip answering every blob size.
fn parse_blob_sizes(output: &str) -> HashMap<String, u64> {
    let mut sizes = HashMap::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (Some(hash), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if kind == "blob" {
            sizes.insert(hash.to_string(), size.parse().unwrap_or(0));
        }
    }
    sizes
}

/// Split one batched `-U3` patch into per-path sections.
fn split_patch(patch: &str) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut starts: Vec<(usize, String)> = Vec::new();
    let marker = "diff --git ";
    for line in patch.lines() {
        if !line.starts_with(marker) {
            continue;
        }
        // `diff --git a/<path> b/<path>`; split at the second ` b/`.
        let rest = &line[marker.len()..];
        let Some(sep) = rest.find(" b/") else {
            continue;
        };
        let path = rest[..sep].strip_prefix("a/").unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        let offset = line.as_ptr() as usize - patch.as_ptr() as usize;
        starts.push((offset, path));
    }
    for index in 0..starts.len() {
        let (start, path) = &starts[index];
        let end = starts
            .get(index + 1)
            .map(|(offset, _)| *offset)
            .unwrap_or(patch.len());
        sections.insert(path.clone(), patch[*start..end].to_string());
    }
    sections
}

fn truncate_bytes(text: &str, max_bytes: u64) -> String {
    let bytes = text.as_bytes();
    if bytes.len() as u64 <= max_bytes {
        return text.to_string();
    }
    String::from_utf8_lossy(&bytes[..max_bytes as usize]).into_owned()
}

/// Whether the file's first `BINARY_SAMPLE_BYTES` contain a NUL byte.
fn has_nul_in_head(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut buffer = [0u8; BINARY_SAMPLE_BYTES];
    let Ok(bytes_read) = file.read(&mut buffer) else {
        return false;
    };
    buffer[..bytes_read].contains(&0)
}

/// Review-level ghost storage backed by a private bare repository.
pub struct GitGhostStorage {
    project: PathBuf,
    side_repo: PathBuf,
    temporary_dir: PathBuf,
    diff_basis_max_bytes: u64,
    seq: u64,
    last_commit: Option<String>,
    repo_ready: bool,
}

impl GitGhostStorage {
    pub fn new(
        project: PathBuf,
        side_repo: PathBuf,
        temporary_dir: PathBuf,
        diff_basis_max_bytes: u64,
    ) -> Self {
        Self {
            project,
            side_repo,
            temporary_dir,
            diff_basis_max_bytes,
            seq: 0,
            last_commit: None,
            repo_ready: false,
        }
    }

    /// Whether `candidate` names a ref inside the undo namespace.
    pub fn is_undo_ref(candidate: &str) -> bool {
        candidate.starts_with(GHOST_REF_PREFIX)
    }

    fn ensure_repo(&mut self) -> Result<()> {
        if self.repo_ready {
            return Ok(());
        }
        let repo_exists = self.side_repo.join("HEAD").exists();
        if !repo_exists {
            if let Some(parent) = self.side_repo.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::create_dir_all(&self.side_repo)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &self.side_repo,
                    std::fs::Permissions::from_mode(0o700),
                );
            }
            let mut cmd = Command::new("git");
            cmd.arg("init")
                .arg("--bare")
                .arg(&self.side_repo)
                .current_dir(&self.project)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let output = cmd.output().map_err(|e| {
                Error::Backend(format!(
                    "git.ghost: init --bare failed: {}",
                    io_detail(&e, "")
                ))
            })?;
            if !output.status.success() {
                return Err(Error::Backend(format!(
                    "git.ghost: init --bare failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        // Reload the ghost chain so fresh storage objects continue it instead
        // of reusing seq 0 and clobbering the existing baseline ref.
        let output = self.git(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                GHOST_REF_PREFIX,
            ],
            None,
            false,
        )?;
        let mut max_seq = 0u64;
        let mut last_commit = None;
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let (Some(refname), Some(commit)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let Some(seq) = refname
                .strip_prefix(GHOST_REF_PREFIX)
                .and_then(|suffix| suffix.parse::<u64>().ok())
            {
                if seq > max_seq {
                    max_seq = seq;
                    last_commit = Some(commit.to_string());
                }
            }
        }
        self.seq = max_seq;
        self.last_commit = last_commit;
        self.repo_ready = true;
        Ok(())
    }

    fn git_command(&self, args: &[&str], index_file: Option<&Path>, identity: bool) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.side_repo)
            .arg("--work-tree")
            .arg(&self.project)
            .args(args)
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The side repo owns every git invocation; ambient repo/index variables
        // must not leak into it.
        cmd.env_remove("GIT_DIR");
        cmd.env_remove("GIT_WORK_TREE");
        cmd.env_remove("GIT_INDEX_FILE");
        if identity {
            for (key, value) in GIT_IDENTITY {
                cmd.env(key, value);
            }
        }
        if let Some(index) = index_file {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd
    }

    fn git(&self, args: &[&str], index_file: Option<&Path>, identity: bool) -> Result<String> {
        self.git_with_input(args, index_file, identity, None)
    }

    fn git_with_input(
        &self,
        args: &[&str],
        index_file: Option<&Path>,
        identity: bool,
        input: Option<&str>,
    ) -> Result<String> {
        let mut child = self
            .git_command(args, index_file, identity)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .spawn()
            .map_err(|error| {
                Error::Backend(format!(
                    "git.ghost: {} failed: {}",
                    args.join(" "),
                    io_detail(&error, "")
                ))
            })?;
        if let Some(data) = input {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            }
        }
        let output = child.wait_with_output().map_err(|error| {
            Error::Backend(format!(
                "git.ghost: {} failed: {}",
                args.join(" "),
                io_detail(&error, "")
            ))
        })?;
        if !output.status.success() {
            return Err(Error::Backend(format!(
                "git.ghost: {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn tree_blobs(&self, tree: &str) -> Result<HashMap<String, String>> {
        if tree == EMPTY_TREE {
            return Ok(HashMap::new());
        }
        let output = self.git(&["ls-tree", "-z", "-r", tree], None, false)?;
        Ok(parse_tree_list(&output))
    }

    fn blob_sizes(&self, hashes: &[String]) -> Result<HashMap<String, u64>> {
        let mut unique: Vec<&str> = hashes.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Ok(HashMap::new());
        }
        let input = format!("{}\n", unique.join("\n"));
        let output =
            self.git_with_input(&["cat-file", "--batch-check"], None, false, Some(&input))?;
        Ok(parse_blob_sizes(&output))
    }

    fn text_patches(
        &self,
        from: &str,
        to: &str,
        paths: &[String],
    ) -> Result<HashMap<String, String>> {
        let mut sections = HashMap::new();
        for chunk in paths.chunks(TEXT_PATCH_CHUNK) {
            let mut args: Vec<String> = vec![
                "-c".into(),
                "core.quotepath=false".into(),
                "diff-tree".into(),
                "--no-renames".into(),
                "-U3".into(),
                from.into(),
                to.into(),
                "--".into(),
            ];
            args.extend(chunk.iter().cloned());
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let patch = self.git(&arg_refs, None, false)?;
            for (path, body) in split_patch(&patch) {
                sections.insert(path, body);
            }
        }
        Ok(sections)
    }

    fn diff_trees_inner(&self, from_tree: &str, to_tree: &str) -> Result<DiffResult> {
        let name_status = self.git(
            &[
                "diff-tree",
                "--no-renames",
                "-z",
                "--name-status",
                from_tree,
                to_tree,
            ],
            None,
            false,
        )?;
        let numstat_output = self.git(
            &[
                "diff-tree",
                "--no-renames",
                "-z",
                "--numstat",
                from_tree,
                to_tree,
            ],
            None,
            false,
        )?;
        let statuses = parse_name_status(&name_status)?;
        let numstat = parse_numstat(&numstat_output);
        let ghost_blobs = self.tree_blobs(from_tree)?;
        let current_blobs = self.tree_blobs(to_tree)?;
        let all_hashes: Vec<String> = ghost_blobs
            .values()
            .chain(current_blobs.values())
            .cloned()
            .collect();
        let sizes = self.blob_sizes(&all_hashes)?;

        let mut changes: Vec<Change> = Vec::new();
        let mut text_paths: Vec<String> = Vec::new();
        for entry in statuses {
            let old_blob = ghost_blobs.get(&entry.path);
            let new_blob = current_blobs.get(&entry.path);
            let old_size = old_blob.and_then(|hash| sizes.get(hash)).copied();
            let new_size = new_blob.and_then(|hash| sizes.get(hash)).copied();
            let stat = numstat.get(&entry.path);
            let too_large = old_size.unwrap_or(0) >= self.diff_basis_max_bytes
                || new_size.unwrap_or(0) >= self.diff_basis_max_bytes;
            let sampled_binary = if new_blob.is_some() {
                has_nul_in_head(&self.work_path(&entry.path)?)
            } else {
                false
            };
            let binary = stat.map(|s| s.binary).unwrap_or(false) || too_large || sampled_binary;
            let kind = match entry.kind {
                'A' => ChangeKind::Added,
                'D' => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            changes.push(Change {
                path: entry.path.clone(),
                kind,
                old_size,
                new_size,
                is_binary: binary,
                added_lines: if binary {
                    0
                } else {
                    stat.map(|s| s.added as usize).unwrap_or(0)
                },
                removed_lines: if binary {
                    0
                } else {
                    stat.map(|s| s.removed as usize).unwrap_or(0)
                },
                text_diff: None,
            });
            if !binary {
                text_paths.push(entry.path);
            }
        }
        if !text_paths.is_empty() {
            let patches = self.text_patches(from_tree, to_tree, &text_paths)?;
            for change in &mut changes {
                if change.is_binary {
                    continue;
                }
                if let Some(patch) = patches.get(&change.path) {
                    change.text_diff = Some(truncate_bytes(patch, self.diff_basis_max_bytes));
                }
            }
        }
        Ok(DiffResult {
            from: Some(from_tree.to_string()),
            to: Some(to_tree.to_string()),
            pending_changes: changes.len(),
            changes,
        })
    }

    fn resolve_tree(&self, treeish: &str) -> Result<String> {
        let output = self
            .git(
                &["rev-parse", "--verify", &format!("{treeish}^{{tree}}")],
                None,
                false,
            )
            .map_err(|_| Error::CheckpointNotFound(treeish.to_string()))?;
        Ok(output.trim().to_string())
    }

    fn require_ref(&self, ghost_ref: &str) -> Result<()> {
        if !Self::is_undo_ref(ghost_ref) {
            return Err(Error::Backend(format!(
                "git.ghost: refusing to touch ref outside the undo namespace: {ghost_ref:?}"
            )));
        }
        Ok(())
    }

    fn work_path(&self, relative_path: &str) -> Result<PathBuf> {
        if relative_path.is_empty() || Path::new(relative_path).is_absolute() {
            return Err(Error::Backend(format!(
                "git.ghost: invalid workspace path {relative_path:?}"
            )));
        }
        let target = self.project.join(relative_path);
        let full = canonicalize_allow_missing(&target)?;
        if !contains_path(&self.project, &full) {
            return Err(Error::Backend(format!(
                "git.ghost: workspace path escapes the project: {relative_path:?}"
            )));
        }
        Ok(target)
    }

    fn temp_index(&self, seq: u64) -> PathBuf {
        self.temporary_dir.join(format!("undo-index-{seq}.idx"))
    }

    fn remove_temp_index(&self, index: &Path) {
        let _ = std::fs::remove_file(index);
    }
}

impl StorageProvider for GitGhostStorage {
    fn snapshot(&mut self) -> Result<String> {
        self.ensure_repo()?;
        let seq = self.seq + 1;
        let index = self.temp_index(seq);
        let outcome = (|| {
            self.git(&["add", "-A"], Some(&index), false)?;
            let tree = self
                .git(&["write-tree"], Some(&index), false)?
                .trim()
                .to_string();
            let mut args: Vec<String> = vec!["commit-tree".into(), tree.clone()];
            if let Some(parent) = &self.last_commit {
                args.push("-p".into());
                args.push(parent.clone());
            }
            args.push("-m".into());
            args.push(format!("deltabox undo snapshot {seq}"));
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let commit = self.git(&arg_refs, None, true)?.trim().to_string();
            if commit.len() != 40 {
                return Err(Error::Backend(format!(
                    "git.ghost: commit-tree returned {commit:?}"
                )));
            }
            let ghost_ref = format!("{GHOST_REF_PREFIX}{seq}");
            self.git(&["update-ref", &ghost_ref, &commit], None, false)?;
            self.seq = seq;
            self.last_commit = Some(commit);
            Ok(ghost_ref)
        })();
        self.remove_temp_index(&index);
        outcome
    }

    fn diff(&self, ghost_ref: &str) -> Result<DiffResult> {
        self.require_ref(ghost_ref)?;
        let ghost_tree = self.resolve_tree(&format!("{ghost_ref}^{{tree}}"))?;
        let index = self.temp_index(0);
        let outcome = (|| {
            self.git(&["add", "-A"], Some(&index), false)?;
            let current_tree = self
                .git(&["write-tree"], Some(&index), false)?
                .trim()
                .to_string();
            self.diff_trees_inner(&ghost_tree, &current_tree)
        })();
        self.remove_temp_index(&index);
        outcome
    }

    fn diff_trees(&self, from: &str, to: &str) -> Result<DiffResult> {
        let from_tree = self.resolve_tree(from)?;
        let to_tree = self.resolve_tree(to)?;
        self.diff_trees_inner(&from_tree, &to_tree)
    }

    fn restore(&mut self, ghost_ref: &str) -> Result<()> {
        self.require_ref(ghost_ref)?;
        let commit = self
            .git(
                &["rev-parse", "--verify", &format!("{ghost_ref}^{{commit}}")],
                None,
                false,
            )
            .map_err(|_| Error::CheckpointNotFound(ghost_ref.to_string()))?
            .trim()
            .to_string();
        let index = self.temp_index(0);
        let outcome = (|| {
            self.git(&["read-tree", &commit], Some(&index), false)?;
            self.git(&["checkout-index", "-a", "-f"], Some(&index), false)?;
            let others = self.git(
                &["ls-files", "--others", "--exclude-standard", "-z"],
                Some(&index),
                false,
            )?;
            for path in others.split('\0') {
                if path.is_empty() {
                    continue;
                }
                let target = self.work_path(path)?;
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.git(&["update-ref", "-d", ghost_ref], None, false)?;
            Ok(())
        })();
        self.remove_temp_index(&index);
        outcome
    }

    fn drop_ref(&mut self, ghost_ref: &str) -> Result<()> {
        self.require_ref(ghost_ref)?;
        self.git(&["update-ref", "-d", ghost_ref], None, false)?;
        Ok(())
    }

    fn list_refs(&self) -> Result<Vec<String>> {
        if !self.side_repo.join("HEAD").exists() {
            return Ok(Vec::new());
        }
        let output = self.git(
            &["for-each-ref", "--format=%(refname)", GHOST_REF_PREFIX],
            None,
            false,
        )?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn close(&mut self) -> Result<()> {
        self.repo_ready = false;
        match std::fs::remove_dir_all(&self.side_repo) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn io_detail(error: &std::io::Error, stderr: &str) -> String {
    let detail = stderr.trim();
    if detail.is_empty() {
        error.to_string()
    } else {
        detail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn side_git(side: &Path, args: &[&str]) -> String {
        let mut full = vec!["--git-dir", side.to_str().unwrap()];
        full.extend_from_slice(args);
        run_git(&full)
    }

    struct Harness {
        project: PathBuf,
        side: PathBuf,
        storage: GitGhostStorage,
        _root: tempfile::TempDir,
    }

    fn setup(project_is_repo: bool) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let side = root.path().join("side.git");
        let tmp = root.path().join("tmp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        if project_is_repo {
            run_git(&["init", project.to_str().unwrap()]);
            run_git(&[
                "-C",
                project.to_str().unwrap(),
                "config",
                "user.email",
                "t@t",
            ]);
            run_git(&["-C", project.to_str().unwrap(), "config", "user.name", "t"]);
            std::fs::write(project.join("baseline.txt"), "baseline\n").unwrap();
            run_git(&["-C", project.to_str().unwrap(), "add", "baseline.txt"]);
            run_git(&["-C", project.to_str().unwrap(), "commit", "-m", "baseline"]);
        }
        let storage = GitGhostStorage::new(
            project.clone(),
            side.clone(),
            tmp.clone(),
            GHOST_DIFF_BASIS_MAX_BYTES,
        );
        Harness {
            project,
            side,
            storage,
            _root: root,
        }
    }

    #[test]
    fn snapshot_isolates_side_repo_and_keeps_project_git_untouched() {
        let mut harness = setup(true);
        std::fs::write(
            harness.project.join("baseline.txt"),
            "baseline\ndirty change\n",
        )
        .unwrap();
        std::fs::write(harness.project.join("new.txt"), "untracked\n").unwrap();

        let head_before = run_git(&["-C", harness.project.to_str().unwrap(), "rev-parse", "HEAD"]);
        let status_before =
            run_git(&["-C", harness.project.to_str().unwrap(), "status", "--short"]);

        let ghost_ref = harness.storage.snapshot().unwrap();
        assert_eq!(ghost_ref, format!("{GHOST_REF_PREFIX}1"));

        assert_eq!(
            run_git(&["-C", harness.project.to_str().unwrap(), "rev-parse", "HEAD"]),
            head_before
        );
        assert_eq!(
            run_git(&["-C", harness.project.to_str().unwrap(), "status", "--short"]),
            status_before
        );
        assert_eq!(
            run_git(&[
                "-C",
                harness.project.to_str().unwrap(),
                "diff",
                "--cached",
                "--quiet",
            ]),
            ""
        );

        // The ghost lives only in the side repo, pinned by the undo ref alone.
        let refs = side_git(&harness.side, &["for-each-ref", "--format=%(refname)"]);
        assert_eq!(refs.trim(), ghost_ref);
        let commit = side_git(
            &harness.side,
            &["rev-parse", "--verify", &format!("{ghost_ref}^{{commit}}")],
        )
        .trim()
        .to_string();
        assert_eq!(commit.len(), 40);
        let missing = Command::new("git")
            .args([
                "-C",
                harness.project.to_str().unwrap(),
                "cat-file",
                "-e",
                &format!("{commit}^{{commit}}"),
            ])
            .output()
            .unwrap();
        assert!(!missing.status.success());

        let tree = side_git(
            &harness.side,
            &["rev-parse", "--verify", &format!("{ghost_ref}^{{tree}}")],
        )
        .trim()
        .to_string();
        let mut names: Vec<String> =
            side_git(&harness.side, &["ls-tree", "-r", "--name-only", &tree])
                .split_whitespace()
                .map(str::to_string)
                .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["baseline.txt".to_string(), "new.txt".to_string()]
        );
        let first = side_git(&harness.side, &["ls-tree", "-r", &tree])
            .lines()
            .next()
            .unwrap()
            .to_string();
        let blob = first
            .split('\t')
            .next()
            .unwrap()
            .split(' ')
            .nth(2)
            .unwrap()
            .to_string();
        assert_eq!(
            side_git(&harness.side, &["cat-file", "blob", &blob]),
            "baseline\ndirty change\n"
        );
    }

    #[test]
    fn restore_rewinds_workspace_and_deletes_review_untracked() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "one\n").unwrap();
        std::fs::create_dir_all(harness.project.join("sub")).unwrap();
        std::fs::write(harness.project.join("sub/b.txt"), "two\n").unwrap();

        let ghost_ref = harness.storage.snapshot().unwrap();
        std::fs::write(harness.project.join("a.txt"), "changed\n").unwrap();
        std::fs::write(harness.project.join("sub/b.txt"), "during review\n").unwrap();
        std::fs::write(harness.project.join("created.txt"), "new\n").unwrap();
        std::fs::write(harness.project.join("sub/extra.txt"), "extra\n").unwrap();

        harness.storage.restore(&ghost_ref).unwrap();

        assert_eq!(
            std::fs::read_to_string(harness.project.join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(harness.project.join("sub/b.txt")).unwrap(),
            "two\n"
        );
        let mut sub: Vec<String> = std::fs::read_dir(harness.project.join("sub"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        sub.sort();
        assert_eq!(sub, vec!["b.txt".to_string()]);
        let mut root: Vec<String> = std::fs::read_dir(&harness.project)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        root.sort();
        assert_eq!(root, vec!["a.txt".to_string(), "sub".to_string()]);
        assert_eq!(side_git(&harness.side, &["for-each-ref"]).trim(), "");
    }

    #[test]
    fn restore_keeps_ignored_files() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join(".gitignore"), "ignored*.txt\n").unwrap();
        std::fs::write(harness.project.join("tracked.txt"), "keep\n").unwrap();
        std::fs::write(harness.project.join("ignored.txt"), "ignored\n").unwrap();

        let ghost_ref = harness.storage.snapshot().unwrap();
        std::fs::write(harness.project.join("tracked.txt"), "mutated\n").unwrap();
        std::fs::write(harness.project.join("ignored.txt"), "still ignored\n").unwrap();
        std::fs::write(harness.project.join("ignored2.txt"), "new ignored\n").unwrap();

        harness.storage.restore(&ghost_ref).unwrap();

        assert_eq!(
            std::fs::read_to_string(harness.project.join("tracked.txt")).unwrap(),
            "keep\n"
        );
        // Ignored files are outside the ghost contract: they stay.
        assert_eq!(
            std::fs::read_to_string(harness.project.join("ignored.txt")).unwrap(),
            "still ignored\n"
        );
        assert_eq!(
            std::fs::read_to_string(harness.project.join("ignored2.txt")).unwrap(),
            "new ignored\n"
        );
    }

    #[test]
    fn commit_drops_only_the_ghost_ref() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "x\n").unwrap();
        let ghost_ref = harness.storage.snapshot().unwrap();
        assert_eq!(
            side_git(&harness.side, &["for-each-ref", "--format=%(refname)"]).trim(),
            ghost_ref
        );

        harness.storage.drop_ref(&ghost_ref).unwrap();

        assert_eq!(side_git(&harness.side, &["for-each-ref"]).trim(), "");
        let missing = Command::new("git")
            .args([
                "--git-dir",
                harness.side.to_str().unwrap(),
                "rev-parse",
                "--verify",
                &ghost_ref,
            ])
            .output()
            .unwrap();
        assert!(!missing.status.success());
    }

    #[test]
    fn snapshots_chain_to_previous_ghost() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "one\n").unwrap();
        let first = harness.storage.snapshot().unwrap();
        std::fs::write(harness.project.join("b.txt"), "two\n").unwrap();
        let second = harness.storage.snapshot().unwrap();

        let first_commit = side_git(
            &harness.side,
            &["rev-parse", "--verify", &format!("{first}^{{commit}}")],
        )
        .trim()
        .to_string();
        assert_eq!(
            side_git(
                &harness.side,
                &["rev-parse", "--verify", &format!("{second}^")]
            )
            .trim(),
            first_commit
        );
        let missing = Command::new("git")
            .args([
                "--git-dir",
                harness.side.to_str().unwrap(),
                "rev-parse",
                "--verify",
                &format!("{first_commit}^"),
            ])
            .output()
            .unwrap();
        assert!(!missing.status.success());
    }

    #[test]
    fn maps_git_diff_to_deltabox_change_shape() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(harness.project.join("mod.txt"), "before\n").unwrap();
        std::fs::write(harness.project.join("del.txt"), "gone\n").unwrap();
        std::fs::write(harness.project.join("bin.dat"), "x").unwrap();
        let ghost_ref = harness.storage.snapshot().unwrap();

        std::fs::write(harness.project.join("mod.txt"), "before\nafter\n").unwrap();
        std::fs::remove_file(harness.project.join("del.txt")).unwrap();
        std::fs::write(harness.project.join("added.txt"), "new lines\n\n").unwrap();
        std::fs::write(harness.project.join("bin.dat"), "x\u{0}y").unwrap();

        let diff = harness.storage.diff(&ghost_ref).unwrap();
        assert!(diff.from.as_deref().unwrap().len() == 40);
        assert!(diff.to.as_deref().unwrap().len() == 40);
        assert_eq!(diff.pending_changes, 4);
        let by_path: HashMap<String, Change> = diff
            .changes
            .into_iter()
            .map(|c| (c.path.clone(), c))
            .collect();
        let modified = by_path.get("mod.txt").unwrap();
        assert_eq!(modified.kind, ChangeKind::Modified);
        assert_eq!(modified.old_size, Some(7));
        assert_eq!(modified.new_size, Some(13));
        assert!(!modified.is_binary);
        assert_eq!(modified.added_lines, 1);
        assert_eq!(modified.removed_lines, 0);
        assert!(modified.text_diff.as_deref().unwrap().contains("+after"));
        let deleted = by_path.get("del.txt").unwrap();
        assert_eq!(deleted.kind, ChangeKind::Deleted);
        assert_eq!(deleted.old_size, Some(5));
        assert_eq!(deleted.new_size, None);
        assert_eq!(deleted.removed_lines, 1);
        let added = by_path.get("added.txt").unwrap();
        assert_eq!(added.kind, ChangeKind::Added);
        assert_eq!(added.old_size, None);
        assert_eq!(added.new_size, Some(11));
        assert_eq!(added.added_lines, 2);
        let binary = by_path.get("bin.dat").unwrap();
        assert_eq!(binary.kind, ChangeKind::Modified);
        assert!(binary.is_binary);
        assert!(binary.text_diff.is_none());
        assert!(!by_path.contains_key("keep.txt"));
    }

    #[test]
    fn reopened_storage_continues_existing_ghost_chain() {
        // A fresh storage object (new CLI process) must reload seq/last_commit
        // from the side repo instead of clobbering the existing baseline ref.
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let side = root.path().join("side.git");
        let tmp = root.path().join("tmp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        let mut first = GitGhostStorage::new(
            project.clone(),
            side.clone(),
            tmp.clone(),
            GHOST_DIFF_BASIS_MAX_BYTES,
        );
        std::fs::write(project.join("a.txt"), "one\n").unwrap();
        let baseline = first.snapshot().unwrap();
        assert_eq!(baseline, format!("{GHOST_REF_PREFIX}1"));
        std::fs::write(project.join("b.txt"), "two\n").unwrap();
        let second = first.snapshot().unwrap();
        assert_eq!(second, format!("{GHOST_REF_PREFIX}2"));
        drop(first);

        let mut reopened = GitGhostStorage::new(
            project.clone(),
            side.clone(),
            tmp.clone(),
            GHOST_DIFF_BASIS_MAX_BYTES,
        );
        let next = reopened.snapshot().unwrap();
        assert_eq!(next, format!("{GHOST_REF_PREFIX}3"));
        let second_commit = side_git(
            &side,
            &["rev-parse", "--verify", "refs/deltabox/undo/2^{commit}"],
        )
        .trim()
        .to_string();
        assert_eq!(
            side_git(&side, &["rev-parse", "--verify", &format!("{next}^")]).trim(),
            second_commit
        );
    }

    #[test]
    fn removes_side_repo_on_close() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "x\n").unwrap();
        harness.storage.snapshot().unwrap();
        harness.storage.close().unwrap();
        assert!(!harness.side.exists());
    }

    #[test]
    fn rejects_refs_outside_undo_namespace() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "x\n").unwrap();
        harness.storage.snapshot().unwrap();
        assert!(harness.storage.drop_ref("refs/heads/main").is_err());
        assert!(harness.storage.restore("refs/heads/main").is_err());
        assert!(harness.storage.diff("refs/heads/main").is_err());
    }

    #[test]
    fn rejects_workspace_path_escape() {
        let mut harness = setup(false);
        std::fs::write(harness.project.join("a.txt"), "x\n").unwrap();
        harness.storage.snapshot().unwrap();
        // A malicious ls-files-style path escaping the project must be refused.
        let error = harness.storage.work_path("../outside.txt").unwrap_err();
        assert!(error.to_string().contains("escapes"));
    }
}
