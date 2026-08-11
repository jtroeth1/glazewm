//! Parsing and window-distribution for `columns` specs.
//!
//! Pure layout math with no dependency on the window tree, so the
//! assignment can be unit-tested with plain indices in place of live
//! windows.

/// One column in a `columns` spec.
#[derive(Debug, PartialEq)]
pub(super) enum ColumnKind {
  /// The wide center column holding the focused window.
  Center,
  /// A stack of exactly `n` windows.
  Fixed(usize),
  /// A stack claiming an even share of the windows left over after the
  /// fixed columns take theirs.
  Star,
}

/// Parses a comma-separated column `spec` into an ordered list of column
/// kinds, left-to-right. A number token is that many stacked windows, `*`
/// is a leftover-sharing stack, and `C` is the wide center.
///
/// Errors on an unrecognised token, a zero fixed count, or a spec that
/// does not contain exactly one `C`.
pub(super) fn parse_columns_spec(
  spec: &str,
) -> anyhow::Result<Vec<ColumnKind>> {
  let mut kinds = Vec::new();
  for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
    if token.eq_ignore_ascii_case("c") {
      kinds.push(ColumnKind::Center);
    } else if token == "*" {
      kinds.push(ColumnKind::Star);
    } else {
      let count = token.parse::<usize>().map_err(|_| {
        anyhow::anyhow!("Column `{token}` must be a number, `*`, or `C`.")
      })?;
      if count == 0 {
        anyhow::bail!("Column count must be at least 1.");
      }
      kinds.push(ColumnKind::Fixed(count));
    }
  }

  if kinds
    .iter()
    .filter(|k| matches!(k, ColumnKind::Center))
    .count()
    != 1
  {
    anyhow::bail!("Column spec must contain exactly one `C`.");
  }

  Ok(kinds)
}

/// Distributes `center` and the `rest` of the windows into columns per
/// the parsed `kinds`.
///
/// `center` fills the `C` column. The `rest` are dealt one per column
/// across the non-center columns left-to-right, then a second row, and so
/// on — a fixed column drops out of the deal once it holds its count, a
/// `*` column never does. So `2,C,*` with five leftovers fills as
/// `[1,3] C [2,4,5]`.
///
/// Two properties matter, and this is the only deal order that has both:
///
/// - **Prefix-stable.** Leftover window `i` lands in the same column no
///   matter how many leftovers there are, so opening or closing a window
///   never shuffles the others between columns. The previous
///   even-share-per-column split moved windows across columns every time
///   the count changed.
/// - **Invertible.** Reading the resulting grid back in row-major order
///   (see [`ColumnGrid::windows`]) reproduces this exact sequence, so
///   applying a layout to its own output is a no-op. Reading is how the
///   window order is recovered — there is no stored order to consult — so
///   without this, every reapply would permute the windows.
///
/// Invertibility is why there is no left/right bias knob: dealing from
/// the right, or from any offset other than the leftmost column, is not
/// recoverable from the grid, and a layout that permutes its own output
/// is exactly the non-determinism this module exists to remove. Mirroring
/// is expressed by reversing the spec instead (see `reverse_spec`).
///
/// Any windows still unplaced (every column is a fixed one and the counts
/// under-specify the total) are appended to the last non-center column,
/// so nothing is dropped.
///
/// Generic over the item so the assignment can be unit-tested with plain
/// indices in place of live windows.
pub(super) fn distribute_columns<T: Clone>(
  kinds: &[ColumnKind],
  center: T,
  rest: Vec<T>,
) -> Vec<Vec<T>> {
  let mut columns: Vec<Vec<T>> =
    kinds.iter().map(|_| Vec::new()).collect();

  // The non-center columns paired with their capacity, in deal order.
  let dealt = kinds
    .iter()
    .enumerate()
    .filter_map(|(index, kind)| match kind {
      ColumnKind::Center => {
        columns[index].push(center.clone());
        None
      }
      ColumnKind::Fixed(count) => Some((index, Some(*count))),
      ColumnKind::Star => Some((index, None)),
    })
    .collect::<Vec<_>>();

  if dealt.is_empty() {
    return columns;
  }

  // Deal row by row, skipping any fixed column that is already full.
  let mut rest = rest.into_iter();
  for row in 0.. {
    let mut placed = false;

    for &(index, capacity) in &dealt {
      if capacity.is_some_and(|capacity| row >= capacity) {
        continue;
      }
      let Some(window) = rest.next() else { break };
      columns[index].push(window);
      placed = true;
    }

    // Either everything is placed or every column is full.
    if !placed {
      break;
    }
  }

  // Every column is a full fixed column but windows remain: keep them
  // rather than dropping them off-screen.
  let remaining = rest.collect::<Vec<_>>();
  if !remaining.is_empty() {
    let (last, _) = dealt[dealt.len() - 1];
    columns[last].extend(remaining);
  }

  columns
}

/// The windows of a distributed grid in the order
/// [`distribute_columns`] dealt them: row-major, left to right.
///
/// The inverse of [`distribute_columns`], and the order the container
/// tree is read back in.
pub(super) fn row_major<T: Clone>(columns: &[Vec<T>]) -> Vec<T> {
  let depth = columns.iter().map(Vec::len).max().unwrap_or(0);

  (0..depth)
    .flat_map(|row| columns.iter().filter_map(move |col| col.get(row)))
    .cloned()
    .collect()
}

/// The width fraction of each column: the center takes `center_fraction`
/// and the remaining width is divided evenly across the non-center
/// columns.
#[allow(clippy::cast_precision_loss)]
pub(super) fn column_widths(
  kinds: &[ColumnKind],
  center_fraction: f32,
) -> Vec<f32> {
  let non_center = kinds.len().saturating_sub(1);
  let side = if non_center > 0 {
    (1.0 - center_fraction) / non_center as f32
  } else {
    0.0
  };

  kinds
    .iter()
    .map(|kind| match kind {
      ColumnKind::Center => center_fraction,
      _ => side,
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::{
    column_widths, distribute_columns, parse_columns_spec, row_major,
    ColumnKind,
  };

  #[test]
  fn parses_star_center_star() {
    assert_eq!(
      parse_columns_spec("*,C,*").unwrap(),
      vec![ColumnKind::Star, ColumnKind::Center, ColumnKind::Star]
    );
  }

  #[test]
  fn parses_explicit_and_lowercase_center() {
    assert_eq!(
      parse_columns_spec("2,1,c,3").unwrap(),
      vec![
        ColumnKind::Fixed(2),
        ColumnKind::Fixed(1),
        ColumnKind::Center,
        ColumnKind::Fixed(3),
      ]
    );
  }

  #[test]
  fn rejects_bad_specs() {
    // No center.
    assert!(parse_columns_spec("*,*").is_err());
    // More than one center.
    assert!(parse_columns_spec("C,C").is_err());
    // Zero fixed count.
    assert!(parse_columns_spec("0,C").is_err());
    // Unparseable token.
    assert!(parse_columns_spec("x,C").is_err());
  }

  #[test]
  fn deals_side_windows_row_by_row() {
    // `*,C,*` with 4 side windows → dealt alternately, left first.
    let kinds = parse_columns_spec("*,C,*").unwrap();
    let columns = distribute_columns(&kinds, 0, vec![1, 2, 3, 4]);
    assert_eq!(columns, vec![vec![1, 3], vec![0], vec![2, 4]]);
  }

  #[test]
  fn row_major_inverts_the_deal() {
    // Reading the grid back row-major must reproduce the sequence that
    // was dealt into it, otherwise reapplying a layout to its own output
    // permutes the windows.
    for spec in ["*,C,*", "C,*", "2,C,*", "1,2,C,*", "*,C"] {
      let kinds = parse_columns_spec(spec).unwrap();

      for count in 0..=8 {
        let rest = (1..=count).collect::<Vec<_>>();
        let columns = distribute_columns(&kinds, 0, rest.clone());

        // The center drops out of the read; the rest come back in order.
        let read = row_major(&columns)
          .into_iter()
          .filter(|window| *window != 0)
          .collect::<Vec<_>>();
        assert_eq!(read, rest, "spec `{spec}` with {count} side windows");

        // Applying the layout to what was read back is a no-op.
        assert_eq!(distribute_columns(&kinds, 0, read), columns);
      }
    }
  }

  #[test]
  fn growing_the_window_count_never_moves_a_window() {
    // The regression this guards: with the old even-share-per-column
    // split, going from 1 to 2 side windows moved the existing window
    // from the left stack to the right (or vice versa), so windows
    // visibly jumped columns every time one was opened or closed.
    for spec in ["*,C,*", "C,*", "2,C,*", "1,2,C,*"] {
      let kinds = parse_columns_spec(spec).unwrap();
      let mut placements = Vec::new();

      for count in 1..=6 {
        let rest = (1..=count).collect::<Vec<_>>();
        let columns = distribute_columns(&kinds, 0, rest);

        let column_of = |window: i32| {
          columns.iter().position(|col| col.contains(&window))
        };

        placements.push((1..=count).map(column_of).collect::<Vec<_>>());
      }

      // Every window keeps the column it had at the smaller count.
      for pair in placements.windows(2) {
        assert_eq!(pair[0][..], pair[1][..pair[0].len()], "spec `{spec}`");
      }
    }
  }

  #[test]
  fn appends_unplaced_windows_to_last_non_center_column() {
    // `1,C` places one window in the fixed column and no `*` to absorb the
    // rest, so the leftovers land in the last non-center column.
    let kinds = parse_columns_spec("1,C").unwrap();
    let columns = distribute_columns(&kinds, 0, vec![1, 2, 3]);
    assert_eq!(columns, vec![vec![1, 2, 3], vec![0]]);
  }

  #[test]
  fn fixed_column_stops_taking_windows_at_its_count() {
    // `2,C,*`: the fixed column takes exactly two windows, dealt on the
    // first two rows, and the `*` column absorbs the remaining three.
    let kinds = parse_columns_spec("2,C,*").unwrap();
    let columns = distribute_columns(&kinds, 0, vec![1, 2, 3, 4, 5]);
    assert_eq!(columns, vec![vec![1, 3], vec![0], vec![2, 4, 5]]);
  }

  #[test]
  fn center_only_spec_places_just_the_center() {
    let kinds = parse_columns_spec("C").unwrap();
    let columns = distribute_columns(&kinds, 0, vec![]);
    assert_eq!(columns, vec![vec![0]]);
  }

  #[test]
  fn widths_split_remainder_evenly() {
    let kinds = parse_columns_spec("*,C,*").unwrap();
    let widths = column_widths(&kinds, 0.6);

    assert_eq!(widths.len(), 3);
    assert!((widths[0] - 0.2).abs() < f32::EPSILON);
    assert!((widths[1] - 0.6).abs() < f32::EPSILON);
    assert!((widths[2] - 0.2).abs() < f32::EPSILON);
  }
}
