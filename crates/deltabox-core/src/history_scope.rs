//! Frozen project-local ignore rules and tracked paths for a history version.
use crate::error::{Error, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SourceScope {
    rules: BTreeMap<String, String>,
    tracked: BTreeSet<String>,
}

impl SourceScope {
    pub fn capture(root: &Path) -> Result<Self> {
        let tracked = tracked_paths(root)?;
        let mut rules = BTreeMap::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|entry| {
                entry.file_name() != ".git"
                    && !(entry.depth() == 1 && entry.file_name() == ".deltabox")
            })
            .build();
        for entry in walker {
            let entry =
                entry.map_err(|error| Error::Backend(format!("history ignore scan: {error}")))?;
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                read_rule(root, &entry.path().join(".gitignore"), &mut rules)?;
            }
        }
        // A tracked file can live below an ignored directory. Record the local
        // rules along its ancestry too; tracked paths themselves always win.
        for relative in &tracked {
            let mut parent = Path::new(relative).parent();
            while let Some(directory) = parent {
                read_rule(root, &root.join(directory).join(".gitignore"), &mut rules)?;
                parent = directory.parent();
            }
        }
        read_rule(root, &root.join(".gitignore"), &mut rules)?;
        let scope = Self { rules, tracked };
        scope.matcher(root)?;
        Ok(scope)
    }

    pub fn matcher(&self, root: &Path) -> Result<ScopeMatcher> {
        let mut rules = Vec::new();
        for (file, contents) in &self.rules {
            validate_relative(file)?;
            if Path::new(file)
                .file_name()
                .is_none_or(|name| name != ".gitignore")
            {
                return Err(Error::Backend("invalid history ignore rule path".into()));
            }
            let directory = Path::new(file).parent().unwrap();
            let mut builder = GitignoreBuilder::new(root.join(directory));
            for line in contents.lines() {
                builder
                    .add_line(Some(root.join(file)), line)
                    .map_err(|error| {
                        Error::Backend(format!("invalid history ignore rule: {error}"))
                    })?;
            }
            rules.push((
                directory.to_path_buf(),
                builder.build().map_err(|error| {
                    Error::Backend(format!("invalid history ignore rules: {error}"))
                })?,
            ));
        }
        rules.sort_by_key(|(directory, _)| std::cmp::Reverse(directory.components().count()));
        for path in &self.tracked {
            validate_relative(path)?;
        }
        Ok(ScopeMatcher {
            root: root.to_path_buf(),
            rules,
            tracked: self.tracked.clone(),
        })
    }

    /// Files/symlinks plus their ancestor metadata. Empty directories and
    /// directories containing only ignored data are outside the version.
    pub fn paths(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let matcher = self.matcher(root)?;
        let mut included = BTreeSet::from([root.to_path_buf()]);
        for item in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| matcher.includes(entry.path(), entry.file_type().is_dir()))
        {
            let item = item.map_err(|error| Error::Io(std::io::Error::other(error)))?;
            if item.file_type().is_dir() {
                continue;
            }
            let mut path = Some(item.path());
            while let Some(current) = path {
                included.insert(current.to_path_buf());
                if current == root {
                    break;
                }
                path = current.parent();
            }
        }
        Ok(included.into_iter().collect())
    }
}

pub(crate) struct ScopeMatcher {
    root: PathBuf,
    rules: Vec<(PathBuf, Gitignore)>,
    tracked: BTreeSet<String>,
}

impl ScopeMatcher {
    pub fn includes(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return true;
        }
        if relative.components().any(|part| part.as_os_str() == ".git")
            || relative
                .components()
                .next()
                .is_some_and(|part| part.as_os_str() == ".deltabox")
        {
            return false;
        }
        if let Some(relative_str) = relative.to_str() {
            if self.tracked.contains(relative_str)
                || (is_dir
                    && self
                        .tracked
                        .iter()
                        .any(|tracked| Path::new(tracked).starts_with(relative)))
            {
                return true;
            }
        }
        // A child cannot be unignored while an ancestor directory remains ignored.
        let mut current = Some(relative);
        while let Some(candidate) = current {
            if candidate.as_os_str().is_empty() {
                break;
            }
            let directory = candidate != relative || is_dir;
            for (base, rules) in &self.rules {
                if candidate.starts_with(base) && candidate != base {
                    let matched = rules.matched(self.root.join(candidate), directory);
                    if matched.is_ignore() {
                        return false;
                    }
                    if !matched.is_none() {
                        break;
                    }
                }
            }
            current = candidate.parent();
        }
        true
    }
}

fn read_rule(root: &Path, path: &Path, rules: &mut BTreeMap<String, String>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let relative = path
                .strip_prefix(root)
                .expect("rule below project")
                .to_str()
                .ok_or_else(|| Error::Unsupported("history requires UTF-8 project paths".into()))?;
            rules.insert(relative.into(), fs::read_to_string(path)?);
        }
        Ok(_) => {} // Git does not follow symlinked .gitignore files.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn tracked_paths(root: &Path) -> Result<BTreeSet<String>> {
    if !root.join(".git").exists() {
        return Ok(BTreeSet::new());
    }
    let output = Command::new("git")
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(root)
        .args(["ls-files", "--cached", "-z", "--", "."])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()?;
    if !output.status.success() {
        if root.join(".git/config").is_file() || root.join(".git").is_file() {
            return Err(Error::Backend(
                "cannot read tracked Git paths for file history".into(),
            ));
        }
        return Ok(BTreeSet::new()); // A metadata fixture is not necessarily a repository.
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path)
                .map_err(|_| Error::Unsupported("history requires UTF-8 tracked paths".into()))?;
            validate_relative(path)?;
            Ok(path.into())
        })
        .collect()
}

fn validate_relative(path: &str) -> Result<()> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::Backend("unsafe history scope path".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), contents).unwrap();
    }

    #[test]
    fn matches_git_for_nested_negations_hidden_and_tracked_ignored_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        write(
            root,
            ".gitignore",
            "target/\n*.log\n!keep.log\n/out/*\n!/out/keep/\n.gitignore\n",
        );
        write(root, "nested/.gitignore", "!local.log\nkeep.log\n");
        write(root, "out/keep/.gitignore", "*.tmp\n!save.tmp\n");
        let paths = [
            "source.rs",
            ".hidden",
            "target/tracked.rs",
            "target/generated.rs",
            "target/keep.log",
            "out/no/file",
            "out/keep/save.tmp",
            "out/keep/drop.tmp",
            "out/keep/code.rs",
            "keep.log",
            "drop.log",
            "nested/local.log",
            "nested/keep.log",
            "nested/drop.log",
            "nested/code.rs",
        ];
        for path in paths {
            write(root, path, path);
        }
        assert!(Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "-f", "target/tracked.rs"])
            .status()
            .unwrap()
            .success());
        let scope = SourceScope::capture(root).unwrap();
        let included = scope.paths(root).unwrap();
        for path in paths {
            let ignored = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["check-ignore", "-q", "--", path])
                .status()
                .unwrap()
                .success();
            assert_eq!(included.contains(&root.join(path)), !ignored, "{path}");
        }
        assert!(!included
            .iter()
            .any(|path| path.starts_with(root.join(".git"))));
        // Global excludes and .git/info/exclude are deliberately not project policy.
        write(root, ".git/info/exclude", "source.rs\n");
        assert!(SourceScope::capture(root)
            .unwrap()
            .paths(root)
            .unwrap()
            .contains(&root.join("source.rs")));
    }

    #[test]
    fn frozen_rules_work_without_git_and_skip_ignored_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, ".gitignore", "target/\n*.log\n");
        write(root, "source.rs", "source");
        fs::create_dir(root.join("target")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.join("target/socket")).unwrap();
        let scope = SourceScope::capture(root).unwrap();
        write(root, ".gitignore", "source.rs\n");
        write(root, "new.log", "outside captured scope");
        let included = scope.paths(root).unwrap();
        assert!(included.contains(&root.join("source.rs")));
        assert!(!included.contains(&root.join("new.log")));
        assert!(!included.contains(&root.join("target")));
        assert_ne!(scope, SourceScope::capture(root).unwrap());
    }
}
