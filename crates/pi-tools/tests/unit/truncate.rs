use super::*;

#[test]
fn head_keeps_the_first_lines() {
    let content = "a\nb\nc\nd\n";
    let truncated = truncate_head(content, 2, 1024);
    assert_eq!(truncated.content, "a\nb\n");
    assert!(truncated.truncated);
    assert_eq!(truncated.total_lines, 4);
}

#[test]
fn tail_keeps_the_last_lines() {
    let content = "a\nb\nc\nd\n";
    let truncated = truncate_tail(content, 2, 1024);
    assert_eq!(truncated.content, "c\nd\n");
    assert!(truncated.truncated);
}

#[test]
fn byte_limit_keeps_whole_lines() {
    // "012345678\n" is 10 bytes; each "aaa\n" is 4. A 15-byte budget fits the
    // first line and one more, then stops before the third.
    let content = "012345678\naaa\nbbb\nccc\n";
    let truncated = truncate_head(content, 100, 15);
    assert_eq!(truncated.content, "012345678\naaa\n");
    assert!(truncated.truncated);
}

#[test]
fn an_overlong_single_line_is_prefix_truncated() {
    let content = "x".repeat(100);
    let truncated = truncate_head(&content, 10, 10);
    assert_eq!(truncated.content.len(), 10);
    assert!(truncated.truncated);
}

#[test]
fn content_within_limits_is_unchanged() {
    let content = "a\nb\n";
    let truncated = truncate_head(content, 100, 1024);
    assert_eq!(truncated.content, content);
    assert!(!truncated.truncated);
    assert_eq!(truncated.total_lines, 2);
}
