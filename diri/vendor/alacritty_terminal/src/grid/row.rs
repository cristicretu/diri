//! Defines the Row type which makes up lines in the grid.

use std::cmp::{max, min};
use std::ops::{Index, IndexMut, Range, RangeFrom, RangeFull, RangeTo, RangeToInclusive};
use std::{ptr, slice};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::grid::GridCell;
use crate::index::Column;
use crate::term::cell::ResetDiscriminant;

/// A row in the grid.
#[derive(Default, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Row<T> {
    inner: Vec<T>,

    /// Maximum number of occupied entries.
    ///
    /// This is the upper bound on the number of elements in the row, which have been modified
    /// since the last reset. All cells after this point are guaranteed to be equal.
    ///
    /// Stored narrow next to `blank_tail` so a row stays four words, which
    /// `Storage::swap` relies on.
    #[cfg_attr(feature = "serde", serde(rename = "occ"))]
    occ: u32,

    /// Every cell at or after `occ` is `T::default()`. Writes through the
    /// index operators only raise `occ`, so they keep it; operations that
    /// place cells past `occ` clear it. History encoding reads it to skip
    /// a row's blank tail without comparing its cells.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) blank_tail: bool,
}

impl<T: PartialEq> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Default> Row<T> {
    /// Create a new terminal row.
    ///
    /// Ideally the `template` should be `Copy` in all performance sensitive scenarios.
    pub fn new(columns: usize) -> Row<T> {
        debug_assert!(columns >= 1);

        let mut inner: Vec<T> = Vec::with_capacity(columns);

        // This is a slightly optimized version of `std::vec::Vec::resize`.
        unsafe {
            let mut ptr = inner.as_mut_ptr();

            for _ in 1..columns {
                ptr::write(ptr, T::default());
                ptr = ptr.offset(1);
            }
            ptr::write(ptr, T::default());

            inner.set_len(columns);
        }

        Row { inner, occ: 0, blank_tail: true }
    }

    /// Increase the number of columns in the row.
    #[inline]
    pub fn grow(&mut self, columns: usize) {
        if self.inner.len() >= columns {
            return;
        }

        self.inner.resize_with(columns, T::default);
    }

    /// Reduce the number of columns in the row.
    ///
    /// This will return all non-empty cells that were removed.
    pub fn shrink(&mut self, columns: usize) -> Option<Vec<T>>
    where
        T: GridCell,
    {
        if self.inner.len() <= columns {
            return None;
        }

        // Split off cells for a new row.
        let mut new_row = self.inner.split_off(columns);
        let index = new_row
            .iter()
            .rposition(|c| !c.is_empty())
            .map_or(0, |i| i + 1);
        new_row.truncate(index);

        self.set_occ(min(self.occ(), columns));

        if new_row.is_empty() {
            None
        } else {
            Some(new_row)
        }
    }

    /// Reset all cells in the row to the `template` cell.
    #[inline]
    pub fn reset<D>(&mut self, template: &T)
    where
        T: ResetDiscriminant<D> + GridCell,
        D: PartialEq,
    {
        debug_assert!(!self.inner.is_empty());

        // Mark all cells as dirty if template cell changed.
        let len = self.inner.len();
        if self.inner[len - 1].discriminant() != template.discriminant() {
            self.set_occ(len);
        }

        // Reset every dirty cell in the row.
        let occ = self.occ();
        for item in &mut self.inner[0..occ] {
            item.reset(template);
        }

        self.blank_tail =
            T::resets_to_default(template) && (self.blank_tail || self.occ() == len);
        self.occ = 0;
    }
}

#[allow(clippy::len_without_is_empty)]
impl<T> Row<T> {
    #[inline]
    pub fn from_vec(vec: Vec<T>, occ: usize) -> Row<T> {
        let blank_tail = occ >= vec.len();
        Row { inner: vec, occ: occ as u32, blank_tail }
    }

    /// `from_vec` of cells whose cells at or after `occ` are known to be
    /// `T::default()`.
    #[inline]
    pub(crate) fn from_vec_blank_tail(vec: Vec<T>, occ: usize) -> Row<T> {
        Row { inner: vec, occ: occ as u32, blank_tail: true }
    }

    /// Cells through the last one that can differ from `T::default()`: the
    /// occupancy when the cells after it are known to be default cells.
    #[inline]
    pub(crate) fn content_bound(&self) -> usize {
        if self.blank_tail { self.occ().min(self.inner.len()) } else { self.inner.len() }
    }

    /// Maximum number of occupied entries (see the field).
    #[inline]
    pub(crate) fn occ(&self) -> usize {
        self.occ as usize
    }

    #[inline]
    pub(crate) fn set_occ(&mut self, occ: usize) {
        self.occ = occ as u32;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn last(&self) -> Option<&T> {
        self.inner.last()
    }

    #[inline]
    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.set_occ(self.inner.len());
        self.inner.last_mut()
    }

    #[inline]
    pub fn append(&mut self, vec: &mut Vec<T>)
    where
        T: GridCell,
    {
        // Appended cells land past the occupancy when it is below the length.
        self.blank_tail &= self.occ() >= self.inner.len();
        self.set_occ(self.occ() + vec.len());
        self.inner.append(vec);
    }

    #[inline]
    pub fn append_front(&mut self, mut vec: Vec<T>) {
        self.set_occ(self.occ() + vec.len());

        vec.append(&mut self.inner);
        self.inner = vec;
    }

    /// Check if all cells in the row are empty.
    #[inline]
    pub fn is_clear(&self) -> bool
    where
        T: GridCell,
    {
        self.inner.iter().all(GridCell::is_empty)
    }

    #[inline]
    pub fn front_split_off(&mut self, at: usize) -> Vec<T> {
        self.set_occ(self.occ().saturating_sub(at));

        let mut split = self.inner.split_off(at);
        std::mem::swap(&mut split, &mut self.inner);
        split
    }
}

impl<'a, T> IntoIterator for &'a Row<T> {
    type IntoIter = slice::Iter<'a, T>;
    type Item = &'a T;

    #[inline]
    fn into_iter(self) -> slice::Iter<'a, T> {
        self.inner.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Row<T> {
    type IntoIter = slice::IterMut<'a, T>;
    type Item = &'a mut T;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.set_occ(self.len());
        self.inner.iter_mut()
    }
}

impl<T> Index<Column> for Row<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Column) -> &T {
        &self.inner[index.0]
    }
}

impl<T> IndexMut<Column> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Column) -> &mut T {
        self.set_occ(max(self.occ(), *index + 1));
        &mut self.inner[index.0]
    }
}

impl<T> Index<Range<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: Range<Column>) -> &[T] {
        &self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> IndexMut<Range<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Range<Column>) -> &mut [T] {
        self.set_occ(max(self.occ(), *index.end));
        &mut self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> Index<RangeTo<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeTo<Column>) -> &[T] {
        &self.inner[..(index.end.0)]
    }
}

impl<T> IndexMut<RangeTo<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeTo<Column>) -> &mut [T] {
        self.set_occ(max(self.occ(), *index.end));
        &mut self.inner[..(index.end.0)]
    }
}

impl<T> Index<RangeFrom<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeFrom<Column>) -> &[T] {
        &self.inner[(index.start.0)..]
    }
}

impl<T> IndexMut<RangeFrom<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeFrom<Column>) -> &mut [T] {
        self.set_occ(self.len());
        &mut self.inner[(index.start.0)..]
    }
}

impl<T> Index<RangeFull> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, _: RangeFull) -> &[T] {
        &self.inner[..]
    }
}

impl<T> IndexMut<RangeFull> for Row<T> {
    #[inline]
    fn index_mut(&mut self, _: RangeFull) -> &mut [T] {
        self.set_occ(self.len());
        &mut self.inner[..]
    }
}

impl<T> Index<RangeToInclusive<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeToInclusive<Column>) -> &[T] {
        &self.inner[..=(index.end.0)]
    }
}

impl<T> IndexMut<RangeToInclusive<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeToInclusive<Column>) -> &mut [T] {
        self.set_occ(max(self.occ(), *index.end + 1));
        &mut self.inner[..=(index.end.0)]
    }
}
