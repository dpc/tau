use super::*;

/// Ensures line-to-byte translation requires a validated line coordinate while
/// retaining the byte-offset fallback and its existing boundary behavior.
#[test]
fn line_index_separates_line_coordinates_from_byte_offsets() {
    let byte_start: fn(&LineIndex, LineNumber, usize) -> usize = LineIndex::byte_start_for_line;
    let index = LineIndex::new(b"one\ntwo");

    assert_eq!(
        byte_start(
            &index,
            LineNumber::new(2).expect("second line is a valid coordinate"),
            7,
        ),
        4
    );
    assert_eq!(
        byte_start(
            &index,
            LineNumber::new(3).expect("EOF insertion slot is a valid coordinate"),
            7,
        ),
        7
    );
}
