use deltabox_core::changeset::{render_diff_text, ChangeKind};
use deltabox_core::protocol::DiffResult;
use std::fmt::Write;

/// Output and path selection shared by session and review diffs.
#[derive(clap::Args, Default, Debug)]
pub struct DiffOutputArgs {
    /// Print the diff as JSON.
    #[arg(long, conflicts_with_all = ["stat", "name_only"])]
    pub json: bool,
    /// Show per-file line counts and byte sizes instead of file contents.
    #[arg(long, conflicts_with = "name_only")]
    pub stat: bool,
    /// Print only changed paths; control characters are JSON-escaped.
    #[arg(long)]
    pub name_only: bool,
    /// Include a project-relative file or directory (repeatable; no glob expansion).
    #[arg(long = "path", value_name = "PATH", value_parser = parse_path_filter)]
    pub paths: Vec<String>,
}

/// Validate lexically, independent of whether the path exists in the current tree.
/// A deleted path can still appear in a diff, so filesystem canonicalization is
/// deliberately avoided. Windows syntax is checked even on Unix hosts.
fn parse_path_filter(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("path must not be empty; use '.' for the entire project".to_owned());
    }
    if value.starts_with('/') || value.contains('\\') || value.contains('\0') {
        return Err("path must be project-relative and use '/' separators".to_owned());
    }

    let mut components = Vec::new();
    for component in value.split('/') {
        match component {
            ".." => return Err("path must not contain '..' components".to_owned()),
            "" | "." => continue,
            _ => components.push(component),
        }
    }
    if let Some(first) = components.first() {
        let bytes = first.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Err("path must not contain a Windows drive prefix".to_owned());
        }
    }

    if components.is_empty() {
        Ok(".".to_owned())
    } else {
        Ok(components.join("/"))
    }
}

fn path_matches(path: &str, filter: &str) -> bool {
    filter == "."
        || path == filter
        || path
            .strip_prefix(filter)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

/// Render a diff after selecting the union of all requested paths.
pub fn render(mut diff: DiffResult, options: &DiffOutputArgs) -> anyhow::Result<String> {
    anyhow::ensure!(
        [options.json, options.stat, options.name_only]
            .into_iter()
            .filter(|enabled| *enabled)
            .count()
            <= 1,
        "--json, --stat, and --name-only are mutually exclusive"
    );
    if !options.paths.is_empty() {
        let filters = options
            .paths
            .iter()
            .map(|value| parse_path_filter(value).map_err(anyhow::Error::msg))
            .collect::<anyhow::Result<Vec<_>>>()?;
        diff.changes.retain(|change| {
            filters
                .iter()
                .any(|filter| path_matches(&change.path, filter))
        });
        diff.pending_changes = diff.changes.len();
    }

    if options.json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&diff)?));
    }
    if options.name_only {
        let mut out = String::new();
        for change in &diff.changes {
            writeln!(out, "{}", display_path(&change.path))?;
        }
        return Ok(out);
    }
    if options.stat {
        return Ok(render_stat(&diff));
    }
    Ok(render_diff_text(&diff))
}

fn display_path(path: &str) -> String {
    if path.chars().any(char::is_control) || path.contains(['"', '\\']) {
        // JSON handles quotes, slashes, and ASCII control characters. Escape
        // DEL and C1 controls as well: JSON permits them literally, but a
        // terminal may interpret them. Keep the result JSON-decodable.
        let quoted = serde_json::to_string(path).expect("strings always serialize as JSON");
        let mut escaped = String::new();
        for character in quoted.chars() {
            if character.is_control() {
                write!(escaped, "\\u{:04x}", character as u32)
                    .expect("writing to a string cannot fail");
            } else {
                escaped.push(character);
            }
        }
        escaped
    } else {
        path.to_owned()
    }
}

fn render_stat(diff: &DiffResult) -> String {
    let mut out = String::new();
    let mut added_lines = 0;
    let mut removed_lines = 0;
    let mut binary_files = 0;
    for change in &diff.changes {
        let kind = match change.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        };
        let path = display_path(&change.path);
        if change.is_binary {
            binary_files += 1;
            let old = change
                .old_size
                .map_or_else(|| "-".to_owned(), |size| size.to_string());
            let new = change
                .new_size
                .map_or_else(|| "-".to_owned(), |size| size.to_string());
            writeln!(out, " {kind} {path} | binary/large ({old} -> {new} bytes)")
                .expect("writing to a string cannot fail");
        } else {
            added_lines += change.added_lines;
            removed_lines += change.removed_lines;
            writeln!(
                out,
                " {kind} {path} | +{} -{}",
                change.added_lines, change.removed_lines
            )
            .expect("writing to a string cannot fail");
        }
    }
    write!(
        out,
        "{} file(s) changed, {added_lines} insertion(s)(+), {removed_lines} deletion(s)(-)",
        diff.changes.len()
    )
    .expect("writing to a string cannot fail");
    if binary_files > 0 {
        write!(out, ", {binary_files} binary/large file(s)")
            .expect("writing to a string cannot fail");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use deltabox_core::changeset::Change;

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        output: DiffOutputArgs,
    }

    fn text_change(path: &str, kind: ChangeKind, added: usize, removed: usize) -> Change {
        Change {
            path: path.to_owned(),
            kind,
            old_size: Some(30),
            new_size: Some(40),
            is_binary: false,
            added_lines: added,
            removed_lines: removed,
            text_diff: Some("-sensitive old content\n+sensitive new content\n".to_owned()),
        }
    }

    fn sample_diff() -> DiffResult {
        DiffResult {
            from: Some("previous".to_owned()),
            to: Some("latest".to_owned()),
            pending_changes: 4,
            changes: vec![
                text_change("src/lib.rs", ChangeKind::Modified, 3, 2),
                text_change("src/nested/new.rs", ChangeKind::Added, 8, 0),
                text_change("src2/other.rs", ChangeKind::Deleted, 0, 5),
                text_change("README.md", ChangeKind::Modified, 1, 1),
            ],
        }
    }

    #[test]
    fn path_parser_normalizes_relative_components_and_trailing_slashes() {
        for (input, expected) in [
            ("./src/", "src"),
            ("./src//./nested/", "src/nested"),
            (".", "."),
            ("././", "."),
            ("a..b", "a..b"),
            ("file with spaces", "file with spaces"),
        ] {
            assert_eq!(parse_path_filter(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn path_parser_rejects_traversal_and_absolute_paths_on_every_platform() {
        for input in [
            "",
            "/tmp",
            "//server/share",
            "..",
            "../src",
            "src/../other",
            "src/..",
            "src\\..\\other",
            "C:\\tmp",
            "C:/tmp",
            "c:relative",
            "./C:/tmp",
            "\\\\server\\share",
            "\\rooted",
            "nul\0byte",
        ] {
            let parsed = TestCli::try_parse_from(["diff", "--path", input]);
            assert!(parsed.is_err(), "unexpectedly accepted {input:?}");
        }
    }

    #[test]
    fn path_filters_match_files_and_directory_boundaries() {
        let output = render(
            sample_diff(),
            &DiffOutputArgs {
                name_only: true,
                paths: vec!["./src/".to_owned()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output, "src/lib.rs\nsrc/nested/new.rs\n");
        assert!(path_matches("src", "src"));
        assert!(!path_matches("src2", "src"));
        assert!(!path_matches("src/lib.rs.bak", "src/lib.rs"));
    }

    #[test]
    fn repeated_path_filters_form_a_union_without_duplicates() {
        let cli = TestCli::try_parse_from([
            "diff",
            "--name-only",
            "--path",
            "src",
            "--path",
            "src/lib.rs",
            "--path",
            "README.md",
        ])
        .unwrap();
        assert_eq!(
            render(sample_diff(), &cli.output).unwrap(),
            "src/lib.rs\nsrc/nested/new.rs\nREADME.md\n"
        );
    }

    #[test]
    fn dot_matches_the_whole_project_and_wildcards_are_literal() {
        let all = DiffOutputArgs {
            name_only: true,
            paths: vec![".".to_owned()],
            ..Default::default()
        };
        assert_eq!(render(sample_diff(), &all).unwrap().lines().count(), 4);
        let wildcard = DiffOutputArgs {
            paths: vec!["src/*".to_owned()],
            ..all
        };
        assert_eq!(render(sample_diff(), &wildcard).unwrap(), "");
    }

    #[test]
    fn filtered_json_has_matching_count_and_preserves_diff_versions() {
        let output = render(
            sample_diff(),
            &DiffOutputArgs {
                json: true,
                paths: vec!["src".to_owned()],
                ..Default::default()
            },
        )
        .unwrap();
        let parsed: DiffResult = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.pending_changes, 2);
        assert_eq!(parsed.changes.len(), 2);
        assert_eq!(parsed.from.as_deref(), Some("previous"));
        assert_eq!(parsed.to.as_deref(), Some("latest"));
    }

    #[test]
    fn stat_counts_only_matching_files_and_never_prints_file_contents() {
        let output = render(
            sample_diff(),
            &DiffOutputArgs {
                stat: true,
                paths: vec!["src".to_owned()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            output,
            " M src/lib.rs | +3 -2\n A src/nested/new.rs | +8 -0\n2 file(s) changed, 11 insertion(s)(+), 2 deletion(s)(-)\n"
        );
        assert!(!output.contains("sensitive"));
    }

    #[test]
    fn stat_shows_binary_sizes_and_missing_sides_without_fabricating_line_counts() {
        let mut diff = sample_diff();
        for (path, kind, old_size, new_size) in [
            ("image.bin", ChangeKind::Modified, Some(4), Some(8)),
            ("new.bin", ChangeKind::Added, None, Some(42)),
            ("gone.bin", ChangeKind::Deleted, Some(17), None),
        ] {
            diff.changes.push(Change {
                path: path.to_owned(),
                kind,
                old_size,
                new_size,
                is_binary: true,
                added_lines: 0,
                removed_lines: 0,
                text_diff: None,
            });
        }
        let output = render(
            diff,
            &DiffOutputArgs {
                stat: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(output.contains(" M image.bin | binary/large (4 -> 8 bytes)\n"));
        assert!(output.contains(" A new.bin | binary/large (- -> 42 bytes)\n"));
        assert!(output.contains(" D gone.bin | binary/large (17 -> - bytes)\n"));
        assert!(output.ends_with(
            "7 file(s) changed, 12 insertion(s)(+), 8 deletion(s)(-), 3 binary/large file(s)\n"
        ));
    }

    #[test]
    fn name_only_and_stat_quote_control_characters_without_injecting_lines() {
        let mut diff = DiffResult::default();
        diff.changes.push(text_change(
            "tab\tline\n\u{1b}[31m\"\\",
            ChangeKind::Added,
            1,
            0,
        ));
        let output = render(
            diff.clone(),
            &DiffOutputArgs {
                name_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output, "\"tab\\tline\\n\\u001b[31m\\\"\\\\\"\n");
        assert_eq!(output.lines().count(), 1);
        assert!(!output.contains("sensitive"));
        let output = render(
            diff,
            &DiffOutputArgs {
                stat: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.lines().count(), 2);
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("sensitive"));
    }

    #[test]
    fn path_quoting_escapes_del_and_c1_controls_and_preserves_unicode() {
        let path = "文件\u{7f}\u{85}\u{9b}[31m";
        let rendered = display_path(path);
        assert_eq!(rendered, "\"文件\\u007f\\u0085\\u009b[31m\"");
        assert!(!rendered.chars().any(char::is_control));
        assert_eq!(serde_json::from_str::<String>(&rendered).unwrap(), path);
        assert_eq!(display_path("文件.rs"), "文件.rs");
    }

    #[test]
    fn empty_filtered_diff_has_consistent_output_in_every_format() {
        for (args, expected) in [
            (
                DiffOutputArgs {
                    name_only: true,
                    ..Default::default()
                },
                "",
            ),
            (
                DiffOutputArgs {
                    stat: true,
                    ..Default::default()
                },
                "0 file(s) changed, 0 insertion(s)(+), 0 deletion(s)(-)\n",
            ),
            (DiffOutputArgs::default(), "0 file(s) changed\n"),
        ] {
            let args = DiffOutputArgs {
                paths: vec!["absent".to_owned()],
                ..args
            };
            assert_eq!(render(sample_diff(), &args).unwrap(), expected);
        }
        let args = DiffOutputArgs {
            json: true,
            paths: vec!["absent".to_owned()],
            ..Default::default()
        };
        let output: DiffResult =
            serde_json::from_str(&render(sample_diff(), &args).unwrap()).unwrap();
        assert_eq!(output.pending_changes, 0);
        assert!(output.changes.is_empty());
    }

    #[test]
    fn output_formats_are_mutually_exclusive_before_any_diff_is_loaded() {
        for flags in [
            ["--json", "--stat"],
            ["--json", "--name-only"],
            ["--stat", "--name-only"],
        ] {
            let error = TestCli::try_parse_from(["diff", flags[0], flags[1]]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn default_text_output_preserves_the_existing_renderer() {
        let diff = sample_diff();
        assert_eq!(
            render(diff.clone(), &DiffOutputArgs::default()).unwrap(),
            render_diff_text(&diff)
        );
    }
}
