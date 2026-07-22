//! Port of `CopyToExtensions.cs` — copies a source slice into a destination
//! slice starting at a given index.
//!
//! The C# indexer assignment throws `ArgumentOutOfRangeException` when the
//! target index is out of range (including a negative index); here that maps to
//! an `Err(CopyToError)`. The index is `isize` so a negative index — exercised
//! by the ported test rows — is representable.

use thiserror::Error;

/// Error returned when a `copy_to` would write outside the destination bounds.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("index out of range: destination has length {len}, attempted to write at {index}")]
pub struct CopyToError {
    /// The out-of-range destination index that was attempted.
    pub index: isize,
    /// The length of the destination slice.
    pub len: usize,
}

/// Copies all elements of `source` into `destination` starting at `index`.
///
/// # Errors
///
/// Returns [`CopyToError`] if any write would fall outside `destination`
/// (matching the C# `ArgumentOutOfRangeException` behavior). Mirroring C#, the
/// copy is aborted at the first out-of-range write; earlier writes may already
/// have landed.
pub fn copy_to<T: Clone>(
    source: &[T],
    destination: &mut [T],
    index: isize,
) -> Result<(), CopyToError> {
    for (i, item) in source.iter().enumerate() {
        let target = index + i.cast_signed();
        if target < 0 || target.cast_unsigned() >= destination.len() {
            return Err(CopyToError {
                index: target,
                len: destination.len(),
            });
        }
        destination[target.cast_unsigned()] = item.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(vec![0, 1, 2, 3, 4, 5], vec![0, 0, 0, 0, 0, 0], 0, vec![0, 1, 2, 3, 4, 5])]
    #[case(vec![0, 1, 2], vec![5, 4, 3, 2, 1, 0], 2, vec![5, 4, 0, 1, 2, 0])]
    fn copy_to_valid_correct(
        #[case] source: Vec<i32>,
        #[case] mut destination: Vec<i32>,
        #[case] index: isize,
        #[case] expected: Vec<i32>,
    ) {
        copy_to(&source, &mut destination, index).expect("copy should succeed");
        assert_eq!(expected, destination);
    }

    #[rstest]
    #[case(vec![0, 1, 2, 3, 4, 5], vec![0, 0, 0, 0, 0, 0], -1)]
    #[case(vec![0, 1, 2], vec![5, 4, 3, 2, 1, 0], 6)]
    #[case(vec![0, 1, 2], vec![], 0)]
    #[case(vec![0, 1, 2, 3, 4, 5], vec![0], 0)]
    #[case(vec![0, 1, 2, 3, 4, 5], vec![0, 0, 0, 0, 0, 0], 1)]
    fn copy_to_invalid_throws_argument_out_of_range_exception(
        #[case] source: Vec<i32>,
        #[case] mut destination: Vec<i32>,
        #[case] index: isize,
    ) {
        let result = copy_to(&source, &mut destination, index);
        assert!(result.is_err());
    }
}
