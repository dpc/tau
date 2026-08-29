#[cfg(debug_assertions)]
use std::cell::Cell as Counter;
use std::ops::Deref;
use std::sync::Arc;

use crate::Cell;

#[cfg(debug_assertions)]
thread_local! {
    static METRICS: Counter<CellRowMetrics> = const { Counter::new(CellRowMetrics::ZERO) };
}

/// Counts immutable row-buffer ownership work on the current thread.
///
/// Counters are available in debug builds, where tests and manual ownership
/// probes run. Release builds remove instrumentation from the row-clone hot
/// path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellRowMetrics {
    /// Newly allocated immutable row buffers.
    pub allocations: usize,
    /// Cheap clones of an existing row-buffer pointer.
    pub pointer_clones: usize,
    /// Cells copied while normalizing caller-constructed rows.
    pub cell_copies: usize,
}

impl CellRowMetrics {
    #[cfg(debug_assertions)]
    const ZERO: Self = Self {
        allocations: 0,
        pointer_clones: 0,
        cell_copies: 0,
    };
}

/// One normalized, immutable, cheaply shared physical terminal row.
#[derive(Debug, PartialEq, Eq)]
pub struct CellRow {
    /// Shared normalized cells for this physical row.
    cells: CellRowStorage,
}

/// Allocation-free empty storage or shared nonempty cells.
#[derive(Debug, PartialEq, Eq)]
enum CellRowStorage {
    /// Canonical allocation-free empty row.
    Empty,
    /// Shared nonempty row cells.
    Cells(Arc<Vec<Cell>>),
}

impl CellRow {
    /// Normalizes and takes ownership of one physical terminal row.
    pub fn new(mut cells: Vec<Cell>) -> Self {
        if cells.is_empty() {
            return Self {
                cells: CellRowStorage::Empty,
            };
        }
        let needs_normalization = cells
            .iter()
            .any(|cell| Cell::sanitized_char(cell.ch) != cell.ch);
        if needs_normalization {
            let copied = cells.len();
            cells = cells.iter().map(Cell::normalized).collect();
            record_cell_copies(copied);
        }
        record_allocation();
        Self {
            cells: CellRowStorage::Cells(Arc::new(cells)),
        }
    }

    /// Copies and normalizes one caller-owned physical terminal row.
    pub(crate) fn copy_normalized(cells: &[Cell]) -> Self {
        if cells.is_empty() {
            return Self::new(Vec::new());
        }
        record_cell_copies(cells.len());
        let cells = cells.iter().map(Cell::normalized).collect();
        record_allocation();
        Self {
            cells: CellRowStorage::Cells(Arc::new(cells)),
        }
    }

    /// Returns debug-build row-buffer counters for the current thread.
    ///
    /// Release builds return `None` because they compile out hot-path
    /// instrumentation.
    pub fn metrics() -> Option<CellRowMetrics> {
        metrics()
    }

    /// Resets debug-build row-buffer counters for the current thread.
    ///
    /// This is a no-op when [`Self::metrics`] returns `None`.
    pub fn reset_metrics() {
        reset_metrics();
    }

    /// Reports whether both rows share storage.
    ///
    /// All allocation-free empty rows share the canonical empty storage.
    pub fn shares_buffer_with(&self, other: &Self) -> bool {
        match (&self.cells, &other.cells) {
            (CellRowStorage::Empty, CellRowStorage::Empty) => true,
            (CellRowStorage::Cells(left), CellRowStorage::Cells(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl Clone for CellRow {
    fn clone(&self) -> Self {
        if matches!(self.cells, CellRowStorage::Cells(_)) {
            record_pointer_clone();
        }
        Self {
            cells: match &self.cells {
                CellRowStorage::Empty => CellRowStorage::Empty,
                CellRowStorage::Cells(cells) => CellRowStorage::Cells(Arc::clone(cells)),
            },
        }
    }
}

impl Deref for CellRow {
    type Target = [Cell];

    fn deref(&self) -> &Self::Target {
        match &self.cells {
            CellRowStorage::Empty => &[],
            CellRowStorage::Cells(cells) => cells.as_slice(),
        }
    }
}

impl AsRef<[Cell]> for CellRow {
    fn as_ref(&self) -> &[Cell] {
        self
    }
}

impl From<Vec<Cell>> for CellRow {
    fn from(cells: Vec<Cell>) -> Self {
        Self::new(cells)
    }
}

impl PartialEq<Vec<Cell>> for CellRow {
    fn eq(&self, other: &Vec<Cell>) -> bool {
        self.as_ref() == other.as_slice()
    }
}

impl CellRowMetrics {
    #[cfg(debug_assertions)]
    fn with_allocation(mut self) -> Self {
        self.allocations += 1;
        self
    }

    #[cfg(debug_assertions)]
    fn with_pointer_clone(mut self) -> Self {
        self.pointer_clones += 1;
        self
    }

    #[cfg(debug_assertions)]
    fn with_cell_copies(mut self, count: usize) -> Self {
        self.cell_copies += count;
        self
    }
}

#[cfg(debug_assertions)]
fn record_allocation() {
    METRICS.set(METRICS.get().with_allocation());
}

#[cfg(not(debug_assertions))]
fn record_allocation() {}

#[cfg(debug_assertions)]
fn record_pointer_clone() {
    METRICS.set(METRICS.get().with_pointer_clone());
}

#[cfg(not(debug_assertions))]
fn record_pointer_clone() {}

#[cfg(debug_assertions)]
fn record_cell_copies(count: usize) {
    METRICS.set(METRICS.get().with_cell_copies(count));
}

#[cfg(not(debug_assertions))]
fn record_cell_copies(_count: usize) {}

#[cfg(debug_assertions)]
fn metrics() -> Option<CellRowMetrics> {
    Some(METRICS.get())
}

#[cfg(not(debug_assertions))]
fn metrics() -> Option<CellRowMetrics> {
    None
}

#[cfg(debug_assertions)]
fn reset_metrics() {
    METRICS.set(CellRowMetrics::ZERO);
}

#[cfg(not(debug_assertions))]
fn reset_metrics() {}
