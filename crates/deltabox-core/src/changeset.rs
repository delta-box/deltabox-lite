use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

pub const DEFAULT_TEXT_SIZE_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
    pub is_binary: bool,
    pub added_lines: usize,
    pub removed_lines: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_diff: Option<String>,
}

/// Binary detection: NUL byte or invalid UTF-8 in the first 8 KiB.
pub fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return true;
    }
    std::str::from_utf8(head).is_err()
}

/// Unified text diff, or None for binary content.
pub fn render_text_diff(old: &[u8], new: &[u8], context: usize) -> Option<String> {
    if is_binary(old) || is_binary(new) {
        return None;
    }
    let old_str = String::from_utf8_lossy(old);
    let new_str = String::from_utf8_lossy(new);
    let diff = TextDiff::from_lines(old_str.as_ref(), new_str.as_ref());
    Some(diff.unified_diff().context_radius(context).to_string())
}

/// Describe a single file change using the v0.1 policy: text diff for small
/// files, whole-file marker for binary or large files.
pub fn describe_change(
    path: &str,
    kind: ChangeKind,
    old_size: Option<u64>,
    new_size: Option<u64>,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    text_size_limit: u64,
) -> Change {
    let too_large =
        old_size.unwrap_or(0) > text_size_limit || new_size.unwrap_or(0) > text_size_limit;
    let binary = too_large
        || match (old_bytes, new_bytes) {
            (Some(o), _) => is_binary(o),
            (None, Some(n)) => is_binary(n),
            (None, None) => false,
        };

    let (added_lines, removed_lines, text_diff) = if binary {
        (0, 0, None)
    } else if let (Some(o), Some(n)) = (old_bytes, new_bytes) {
        if let Some(diff) = render_text_diff(o, n, 3) {
            (
                count_marker_lines(&diff, '+'),
                count_marker_lines(&diff, '-'),
                Some(diff),
            )
        } else {
            (0, 0, None)
        }
    } else {
        (0, 0, None)
    };

    Change {
        path: path.to_string(),
        kind,
        old_size,
        new_size,
        is_binary: binary,
        added_lines,
        removed_lines,
        text_diff,
    }
}

fn count_marker_lines(diff: &str, marker: char) -> usize {
    let header = format!("{marker}{marker}{marker}");
    diff.lines()
        .filter(|l| l.starts_with(marker) && !l.starts_with(&header))
        .count()
}

// Keep ChangeTag import used (future extension points); silence the lint in
// case the compiler sees it as unused in the current build.
#[allow(dead_code)]
fn _tag_name(tag: ChangeTag) -> &'static str {
    match tag {
        ChangeTag::Equal => "equal",
        ChangeTag::Delete => "delete",
        ChangeTag::Insert => "insert",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"\x00\x01\x02"));
        assert!(is_binary(b"\xff\xfe\x00"));
        assert!(!is_binary(b"hello world\n"));
        assert!(!is_binary("你好".as_bytes()));
    }

    #[test]
    fn text_diff_marks_lines() {
        let change = describe_change(
            "a.txt",
            ChangeKind::Modified,
            Some(6),
            Some(12),
            Some(b"hello\n"),
            Some(b"hello\nworld\n"),
            DEFAULT_TEXT_SIZE_LIMIT,
        );
        assert!(!change.is_binary);
        assert_eq!(change.added_lines, 1);
        assert_eq!(change.removed_lines, 0);
        let diff = change.text_diff.unwrap();
        assert!(diff.contains("+world"));
    }

    #[test]
    fn large_and_binary_files_are_marked_not_diffed() {
        let big = describe_change(
            "big.bin",
            ChangeKind::Modified,
            Some(10 * 1024 * 1024),
            Some(11 * 1024 * 1024),
            None,
            None,
            DEFAULT_TEXT_SIZE_LIMIT,
        );
        assert!(big.is_binary);
        assert!(big.text_diff.is_none());

        let bin = describe_change(
            "img.png",
            ChangeKind::Modified,
            Some(4),
            Some(5),
            Some(b"\x89PNG"),
            Some(b"\x89PNG\x00"),
            DEFAULT_TEXT_SIZE_LIMIT,
        );
        assert!(bin.is_binary);
        assert!(bin.text_diff.is_none());
    }
}
