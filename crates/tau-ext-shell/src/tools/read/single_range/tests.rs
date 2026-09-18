use super::*;
use crate::tools::read::{
    LineNumber, MAX_READ_FILE_BYTES, slice_line_ranges, validate_ranges_with_total,
};
use crate::truncate::{MAX_OUTPUT_LINES, truncate_line_oriented};

fn range(start: usize, end: Option<usize>) -> ReadLineRange {
    ReadLineRange {
        start_line: LineNumber::new(start).expect("nonzero test start"),
        end_line: end.map(|end| LineNumber::new(end).expect("nonzero test end")),
    }
}

fn assert_matches_old(input: &[u8], range: ReadLineRange) {
    let old = slice_line_ranges(input, std::slice::from_ref(&range));
    let prepared = Prepared::new(input, &range);
    assert_eq!(prepared.total_lines, old.total_lines);
    assert_eq!(prepared.valid_utf8, old.valid_utf8);
    assert_eq!(prepared.selected_lines, old.line_count);
    assert_eq!(prepared.rendered_bytes, old.content.len());
    assert_eq!(prepared.prefix, old.content);
    assert!(prepared.head.len() + prepared.tail.len() <= MAX_OUTPUT_LINES);
    let expected = truncate_line_oriented(&old.content);
    let actual = prepared.truncate();
    assert_eq!(actual.content, expected.content);
    assert_eq!(actual.was_truncated, expected.was_truncated);
    assert_eq!(actual.total_lines, expected.total_lines);
    assert_eq!(actual.total_bytes, expected.total_bytes);
    assert_eq!(
        validate_ranges_with_total(std::slice::from_ref(&range), old.total_lines, "args")
            .map_err(|error| error.message),
        validate_ranges_with_total(std::slice::from_ref(&range), prepared.total_lines, "args")
            .map_err(|error| error.message),
    );
}

/// Compare manageable old full-render oracles around line and byte boundaries,
/// including source-wide validity and coordinates outside the selected range.
#[test]
fn preparation_matches_full_render_and_shared_truncation() {
    for count in [0, 1, 1_999, 2_000, 2_001, 10_000] {
        assert_matches_old(&vec![b'\n'; count], range(1, None));
    }
    for size in [10_237, 10_238, 10_239, 10_240, 10_241, 30_000] {
        let mut input = vec![b'x'; size];
        input.push(b'\n');
        assert_matches_old(&input, range(1, None));
        input.extend_from_slice(b"tail");
        assert_matches_old(&input, range(1, None));
    }
    for input in [
        b"".as_slice(),
        b"\n",
        b"one",
        b"one\r\ntwo\rthree\nfour",
        b"\xff\nok\n\xfe\r\nlast",
        b"ok\n\xff\nlast\n",
        "🙂\r\né\r終\n".as_bytes(),
    ] {
        for selected in [
            range(1, None),
            range(2, Some(2)),
            range(1, Some(100)),
            range(99, None),
        ] {
            assert_matches_old(input, selected);
        }
    }
}

/// Newline density must not grow the number of retained descriptors, and the
/// saved prefix remains capped even for the accepted ten-MiB source.
#[test]
fn preparation_bounds_retained_state_at_input_cap() {
    for count in [2_001, 100_000, MAX_READ_FILE_BYTES] {
        let input = vec![b'\n'; count];
        let prepared = Prepared::new(&input, &range(1, None));
        assert_eq!(prepared.total_lines, count);
        assert_eq!(prepared.selected_lines, count);
        assert_eq!(prepared.head.len(), TRUNCATED_OUTPUT_HEAD_LINES);
        assert_eq!(prepared.tail.len(), TRUNCATED_OUTPUT_TAIL_LINES);
        assert_eq!(prepared.tail.back().expect("retained tail").number, count);
        assert!(prepared.prefix.len() <= MAX_SAVED_OUTPUT_BYTES);
        if count == MAX_READ_FILE_BYTES {
            assert_eq!(prepared.prefix.len(), MAX_SAVED_OUTPUT_BYTES);
            assert!(prepared.rendered_bytes > prepared.prefix.len());
        }
    }
}

/// Prefix pieces must stop at the first omitted byte, including a multibyte
/// character straddling the cap; subsequent pieces may not fill the gap.
#[test]
fn prefix_matches_utf8_safe_full_render_boundaries() {
    for (padding, suffix) in [
        (MAX_SAVED_OUTPUT_BYTES - 1, ""),
        (MAX_SAVED_OUTPUT_BYTES, ""),
        (MAX_SAVED_OUTPUT_BYTES, "x"),
        (MAX_SAVED_OUTPUT_BYTES - 1, "é\nlater"),
        (MAX_SAVED_OUTPUT_BYTES - 2, "é"),
    ] {
        let mut prepared = Prepared::new(b"", &range(1, None));
        let padding = "x".repeat(padding);
        prepared.append_prefix(&padding);
        for character in suffix.chars() {
            prepared.append_prefix(character.encode_utf8(&mut [0; 4]));
        }
        let full = padding + suffix;
        let mut end = full.len().min(MAX_SAVED_OUTPUT_BYTES);
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        assert_eq!(prepared.prefix, full[..end]);
        assert_eq!(prepared.rendered_bytes, full.len());
    }
}
