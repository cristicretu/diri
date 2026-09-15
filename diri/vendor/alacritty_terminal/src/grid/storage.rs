use std::cmp::max;
use std::mem;
use std::mem::MaybeUninit;
use std::ops::{Index, IndexMut};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::Row;
#[cfg(feature = "compact-history")]
use super::compact::{CompactRows, RowCodec};
use crate::index::Line;

/// Maximum number of buffered lines outside of the grid for performance optimization.
const MAX_CACHE_SIZE: usize = 1_000;

/// Byte budget for newly allocated spare rows, including their row descriptors.
const MAX_CACHE_BYTES: usize = 64 * 1024;

fn cache_rows<T>(columns: usize) -> usize {
    let row_bytes = columns
        .saturating_mul(mem::size_of::<T>())
        .saturating_add(mem::size_of::<Row<T>>());
    (MAX_CACHE_BYTES / row_bytes.max(1)).clamp(1, MAX_CACHE_SIZE)
}

/// A ring buffer for optimizing indexing and rotation.
///
/// The [`Storage::rotate`] and [`Storage::rotate_down`] functions are fast modular additions on
/// the internal [`zero`] field. As compared with [`slice::rotate_left`] which must rearrange items
/// in memory.
///
/// As a consequence, both [`Index`] and [`IndexMut`] are reimplemented for this type to account
/// for the zeroth element not always being at the start of the allocation.
///
/// Because certain [`Vec`] operations are no longer valid on this type, no [`Deref`]
/// implementation is provided. Anything from [`Vec`] that should be exposed must be done so
/// manually.
///
/// [`slice::rotate_left`]: https://doc.rust-lang.org/std/primitive.slice.html#method.rotate_left
/// [`Deref`]: std::ops::Deref
/// [`zero`]: #structfield.zero
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Deserialize))]
#[cfg_attr(
    all(feature = "serde", not(feature = "compact-history")),
    derive(Serialize)
)]
pub struct Storage<T> {
    inner: Vec<Row<T>>,
    #[cfg(feature = "compact-history")]
    #[serde(skip)]
    compact: Option<CompactRows<T>>,

    /// Starting point for the storage of rows.
    ///
    /// This value represents the starting line offset within the ring buffer. The value of this
    /// offset may be larger than the `len` itself, and will wrap around to the start to form the
    /// ring buffer. It represents the bottommost line of the terminal.
    zero: usize,

    /// Number of visible lines.
    visible_lines: usize,

    /// Total number of lines currently active in the terminal (scrollback + visible)
    ///
    /// Shrinking this length allows reducing the number of lines in the scrollback buffer without
    /// having to truncate the raw `inner` buffer.
    /// As long as `len` is bigger than `inner`, it is also possible to grow the scrollback buffer
    /// without any additional insertions.
    len: usize,
}

impl<T: PartialEq> PartialEq for Storage<T> {
    fn eq(&self, other: &Self) -> bool {
        // Both storage buffers need to be truncated and zeroed.
        assert_eq!(self.zero, 0);
        assert_eq!(other.zero, 0);

        #[cfg(feature = "compact-history")]
        if self.compact.is_some() || other.compact.is_some() {
            return self.len == other.len
                && (0..self.len).all(|i| {
                    self[Line(self.visible_lines as i32 - 1 - i as i32)]
                        == other[Line(other.visible_lines as i32 - 1 - i as i32)]
                });
        }
        self.inner == other.inner && self.len == other.len
    }
}

impl<T> Storage<T> {
    #[cfg(feature = "compact-history")]
    pub fn enable_compact(&mut self, codec: RowCodec<T>) {
        if self.compact.is_some() {
            return;
        }
        let columns = self.inner.first().map_or(0, Row::len);
        let rows = self.take_all();
        self.len = rows.len();
        self.zero = 0;
        self.compact = Some(CompactRows::new(rows, self.visible_lines, columns, codec));
    }

    #[cfg(feature = "compact-history")]
    pub fn release_read_cache(&mut self) {
        if let Some(compact) = &mut self.compact {
            compact.release_read_cache();
        }
    }

    #[cfg(feature = "compact-history")]
    pub fn bound_history_bytes(&mut self, budget: usize) {
        if let Some(compact) = &mut self.compact {
            compact.bound_history_bytes(budget);
            self.len = compact.len();
        }
    }

    #[cfg(feature = "compact-history")]
    pub fn history_storage_bytes(&self) -> usize {
        self.compact
            .as_ref()
            .map_or(0, CompactRows::history_storage_bytes)
    }

    #[inline]
    pub fn with_capacity(visible_lines: usize, columns: usize) -> Storage<T>
    where
        T: Default,
    {
        // Initialize visible lines; the scrollback buffer is initialized dynamically.
        let mut inner = Vec::with_capacity(visible_lines);
        inner.resize_with(visible_lines, || Row::new(columns));

        Storage {
            inner,
            #[cfg(feature = "compact-history")]
            compact: None,
            zero: 0,
            visible_lines,
            len: visible_lines,
        }
    }

    /// Increase the number of lines in the buffer.
    #[inline]
    pub fn grow_visible_lines(&mut self, next: usize)
    where
        T: Default,
    {
        // Number of lines the buffer needs to grow.
        let additional_lines = next - self.visible_lines;

        let columns = self[Line(0)].len();
        self.initialize(additional_lines, columns);

        // Update visible lines.
        self.visible_lines = next;
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.set_visible(next);
        }
    }

    /// Decrease the number of lines in the buffer.
    #[inline]
    pub fn shrink_visible_lines(&mut self, next: usize) {
        // Shrink the size without removing any lines.
        let shrinkage = self.visible_lines - next;
        self.shrink_lines(shrinkage);

        // Update visible lines.
        self.visible_lines = next;
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.set_visible(next);
        }
    }

    /// Shrink the number of lines in the buffer.
    #[inline]
    pub fn shrink_lines(&mut self, shrinkage: usize) {
        self.len -= shrinkage;
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.truncate(self.len);
            return;
        }

        // Free memory.
        let columns = self.inner.first().map_or(0, Row::len);
        if self.inner.len() > self.len + cache_rows::<T>(columns) {
            self.truncate();
        }
    }

    /// Truncate the invisible elements from the raw buffer.
    #[inline]
    pub fn truncate(&mut self) {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.release_read_cache();
            return;
        }
        self.rezero();

        self.inner.truncate(self.len);
    }

    /// Dynamically grow the storage buffer at runtime.
    #[inline]
    pub fn initialize(&mut self, additional_rows: usize, columns: usize)
    where
        T: Default,
    {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.initialize(additional_rows, columns);
            self.len += additional_rows;
            return;
        }
        if self.len + additional_rows > self.inner.len() {
            self.rezero();

            let realloc_size = self.inner.len() + max(additional_rows, cache_rows::<T>(columns));
            self.inner.resize_with(realloc_size, || Row::new(columns));
        }

        self.len += additional_rows;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Swap implementation for Row<T>.
    ///
    /// Exploits the known size of Row<T> to produce a slightly more efficient
    /// swap than going through slice::swap.
    ///
    /// The default implementation from swap generates 8 movups and 4 movaps
    /// instructions. This implementation achieves the swap in only 8 movups
    /// instructions.
    pub fn swap(&mut self, a: Line, b: Line) {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            let a = (self.visible_lines as i32 - 1 - a.0) as usize;
            let b = (self.visible_lines as i32 - 1 - b.0) as usize;
            compact.swap(a, b);
            return;
        }
        debug_assert_eq!(mem::size_of::<Row<T>>(), mem::size_of::<usize>() * 4);

        let a = self.compute_index(a);
        let b = self.compute_index(b);

        unsafe {
            // Cast to a qword array to opt out of copy restrictions and avoid
            // drop hazards. Byte array is no good here since for whatever
            // reason LLVM won't optimized it.
            let a_ptr = self.inner.as_mut_ptr().add(a) as *mut MaybeUninit<usize>;
            let b_ptr = self.inner.as_mut_ptr().add(b) as *mut MaybeUninit<usize>;

            // Copy 1 qword at a time.
            //
            // The optimizer unrolls this loop and vectorizes it.
            let mut tmp: MaybeUninit<usize>;
            for i in 0..4 {
                tmp = *a_ptr.offset(i);
                *a_ptr.offset(i) = *b_ptr.offset(i);
                *b_ptr.offset(i) = tmp;
            }
        }
    }

    /// Rotate the grid, moving all lines up/down in history.
    #[inline]
    pub fn rotate(&mut self, count: isize) {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.rotate(count);
            return;
        }
        debug_assert!(count.unsigned_abs() <= self.inner.len());

        let len = self.inner.len();
        self.zero = (self.zero as isize + count + len as isize) as usize % len;
    }

    /// Rotate all existing lines down in history.
    ///
    /// This is a faster, specialized version of [`rotate_left`].
    ///
    /// [`rotate_left`]: https://doc.rust-lang.org/std/vec/struct.Vec.html#method.rotate_left
    #[inline]
    pub fn rotate_down(&mut self, count: usize) {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            compact.rotate(count as isize);
            return;
        }
        self.zero = (self.zero + count) % self.inner.len();
    }

    /// Update the raw storage buffer.
    #[inline]
    pub fn replace_inner(&mut self, vec: Vec<Row<T>>) {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            self.len = vec.len();
            let columns = vec.first().map_or(0, Row::len);
            compact.replace_rows(vec, self.visible_lines, columns);
            return;
        }
        self.len = vec.len();
        self.inner = vec;
        self.zero = 0;
    }

    /// Remove all rows from storage.
    #[inline]
    pub fn take_all(&mut self) -> Vec<Row<T>> {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            self.len = 0;
            return compact.drain_rows();
        }
        self.truncate();

        let mut buffer = Vec::new();

        mem::swap(&mut buffer, &mut self.inner);
        self.len = 0;

        buffer
    }

    /// Compute actual index in underlying storage given the requested index.
    #[inline]
    fn compute_index(&self, requested: Line) -> usize {
        Self::ring_index(
            self.visible_lines,
            self.zero,
            self.len,
            self.inner.len(),
            requested,
        )
    }

    #[inline]
    fn ring_index(
        visible_lines: usize,
        zero: usize,
        len: usize,
        capacity: usize,
        requested: Line,
    ) -> usize {
        debug_assert!(requested.0 < visible_lines as i32);

        let positive = -(requested - visible_lines).0 as usize - 1;

        debug_assert!(positive < len);

        let zeroed = zero + positive;

        // Use if/else instead of remainder here to improve performance.
        //
        // Requires `zeroed` to be smaller than `capacity * 2`,
        // but both `zero` and `requested` are always smaller than `capacity`.
        if zeroed >= capacity {
            zeroed - capacity
        } else {
            zeroed
        }
    }

    /// Rotate the ringbuffer to reset `self.zero` back to index `0`.
    #[inline]
    fn rezero(&mut self) {
        if self.zero == 0 {
            return;
        }

        self.inner.rotate_left(self.zero);
        self.zero = 0;
    }
}

#[cfg(feature = "compact-history")]
impl<T: Serialize> Serialize for Storage<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeSeq, SerializeStruct};

        struct Rows<'a, T>(&'a CompactRows<T>);
        impl<T: Serialize> Serialize for Rows<'_, T> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut rows = serializer.serialize_seq(Some(self.0.len()))?;
                for index in 0..self.0.len() {
                    rows.serialize_element(self.0.row(index))?;
                }
                rows.end()
            }
        }

        let mut state = serializer.serialize_struct("Storage", 4)?;
        if let Some(compact) = &self.compact {
            state.serialize_field("inner", &Rows(compact))?;
        } else {
            state.serialize_field("inner", &self.inner)?;
        }
        state.serialize_field("zero", &self.zero)?;
        state.serialize_field("visible_lines", &self.visible_lines)?;
        state.serialize_field("len", &self.len)?;
        state.end()
    }
}

impl<T> Index<Line> for Storage<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: Line) -> &Self::Output {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &self.compact {
            return compact.row((self.visible_lines as i32 - 1 - index.0) as usize);
        }
        let index = self.compute_index(index);
        &self.inner[index]
    }
}

impl<T> IndexMut<Line> for Storage<T> {
    #[inline]
    fn index_mut(&mut self, index: Line) -> &mut Self::Output {
        #[cfg(feature = "compact-history")]
        if let Some(compact) = &mut self.compact {
            return compact.row_mut((self.visible_lines as i32 - 1 - index.0) as usize);
        }
        let index = Self::ring_index(
            self.visible_lines,
            self.zero,
            self.len,
            self.inner.len(),
            index,
        );
        &mut self.inner[index]
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::GridCell;
    use crate::grid::row::Row;
    use crate::grid::storage::{MAX_CACHE_SIZE, Storage};
    use crate::index::{Column, Line};
    use crate::term::cell::Flags;

    impl GridCell for char {
        fn is_empty(&self) -> bool {
            *self == ' ' || *self == '\t'
        }

        fn reset(&mut self, template: &Self) {
            *self = *template;
        }

        fn flags(&self) -> &Flags {
            unimplemented!();
        }

        fn flags_mut(&mut self) -> &mut Flags {
            unimplemented!();
        }
    }

    #[test]
    fn history_reserve_scales_with_terminal_row_bytes() {
        use super::MAX_CACHE_BYTES;
        use crate::term::cell::Cell;
        for columns in [1, 80, 320, 4096] {
            let mut storage = Storage::<Cell>::with_capacity(24, columns);
            storage.initialize(1, columns);
            let allocated = storage.inner.len() - 24;
            let row_bytes =
                columns * std::mem::size_of::<Cell>() + std::mem::size_of::<Row<Cell>>();
            assert!(allocated == 1 || allocated * row_bytes <= MAX_CACHE_BYTES);
            assert_eq!(storage.len, 25);
            storage[Line(-1)][Column(0)].c = 'H';
            assert_eq!(storage[Line(-1)][Column(0)].c, 'H');
        }
    }

    #[test]
    fn with_capacity() {
        let storage = Storage::<char>::with_capacity(3, 1);

        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.len, 3);
        assert_eq!(storage.zero, 0);
        assert_eq!(storage.visible_lines, 3);
    }

    #[test]
    fn indexing() {
        let mut storage = Storage::<char>::with_capacity(3, 1);

        storage[Line(0)] = filled_row('0');
        storage[Line(1)] = filled_row('1');
        storage[Line(2)] = filled_row('2');

        storage.zero += 1;

        assert_eq!(storage[Line(0)], filled_row('2'));
        assert_eq!(storage[Line(1)], filled_row('0'));
        assert_eq!(storage[Line(2)], filled_row('1'));
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn indexing_above_inner_len() {
        let storage = Storage::<char>::with_capacity(1, 1);
        let _ = &storage[Line(-1)];
    }

    #[test]
    fn rotate() {
        let mut storage = Storage::<char>::with_capacity(3, 1);
        storage.rotate(2);
        assert_eq!(storage.zero, 2);
        storage.shrink_lines(2);
        assert_eq!(storage.len, 1);
        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.zero, 2);
    }

    /// Grow the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   MAX_CACHE_SIZE: \0
    #[test]
    fn grow_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 3,
            len: 3,
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 4,
            len: 4,
        };
        expected
            .inner
            .append(&mut vec![filled_row('\0'); MAX_CACHE_SIZE]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Grow the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: -
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   MAX_CACHE_SIZE: \0
    #[test]
    fn grow_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('-'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 3,
            len: 3,
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 4,
            len: 4,
        };
        expected
            .inner
            .append(&mut vec![filled_row('\0'); MAX_CACHE_SIZE]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: 2
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 2 <- Hidden
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn shrink_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 3,
            len: 3,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2 <- Hidden
    #[test]
    fn shrink_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            zero: 0,
            visible_lines: 3,
            len: 3,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            zero: 0,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer at the start and end of the buffer.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    #[test]
    fn shrink_before_and_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 6,
            len: 6,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Check that when truncating all hidden lines are removed from the raw buffer.
    ///
    /// Before:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn truncate_invisible_lines() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 1,
            len: 2,
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1')],
            zero: 0,
            visible_lines: 1,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Truncate buffer only at the beginning.
    ///
    /// Before:
    ///   0: 1
    ///   1: 2 <- Hidden
    ///   2: 0 <- Zero
    /// After:
    ///   0: 1
    ///   0: 0 <- Zero
    #[test]
    fn truncate_invisible_lines_beginning() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('1'), filled_row('2'), filled_row('0')],
            zero: 2,
            visible_lines: 1,
            len: 2,
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('0'), filled_row('1')],
            zero: 0,
            visible_lines: 1,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// First shrink the buffer and then grow it again.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After Shrinking:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3 <- Hidden
    /// After Growing:
    ///   0: 4
    ///   1: 5
    ///   2: -
    ///   3: 0 <- Zero
    ///   4: 1
    ///   5: 2
    ///   6: 3
    #[test]
    fn shrink_then_grow() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 6,
        };

        // Shrink buffer.
        storage.shrink_lines(3);

        // Make sure the result after shrinking is correct.
        let shrinking_expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 3,
        };
        assert_eq!(storage.inner, shrinking_expected.inner);
        assert_eq!(storage.zero, shrinking_expected.zero);
        assert_eq!(storage.len, shrinking_expected.len);

        // Grow buffer.
        storage.initialize(1, 1);

        // Make sure the previously freed elements are reused.
        let growing_expected = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 4,
        };

        assert_eq!(storage.inner, growing_expected.inner);
        assert_eq!(storage.zero, growing_expected.zero);
        assert_eq!(storage.len, growing_expected.len);
    }

    #[test]
    fn initialize() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 6,
        };

        // Initialize additional lines.
        let init_size = 3;
        storage.initialize(init_size, 1);

        // Generate expected grid.
        let mut expected_inner = vec![
            filled_row('0'),
            filled_row('1'),
            filled_row('2'),
            filled_row('3'),
            filled_row('4'),
            filled_row('5'),
        ];
        let expected_init_size = std::cmp::max(init_size, MAX_CACHE_SIZE);
        expected_inner.append(&mut vec![filled_row('\0'); expected_init_size]);
        let expected_storage = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: expected_inner,
            zero: 0,
            visible_lines: 0,
            len: 9,
        };

        assert_eq!(storage.len, expected_storage.len);
        assert_eq!(storage.zero, expected_storage.zero);
        assert_eq!(storage.inner, expected_storage.inner);
    }

    #[test]
    fn rotate_wrap_zero() {
        let mut storage: Storage<char> = Storage {
            #[cfg(feature = "compact-history")]
            compact: None,
            inner: vec![filled_row('-'), filled_row('-'), filled_row('-')],
            zero: 2,
            visible_lines: 0,
            len: 3,
        };

        storage.rotate(2);

        assert!(storage.zero < storage.inner.len());
    }

    fn filled_row(content: char) -> Row<char> {
        let mut row = Row::new(1);
        row[Column(0)] = content;
        row
    }
}
