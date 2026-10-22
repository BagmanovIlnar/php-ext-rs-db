//! Result cursors.
//!
//! * [`RowCursor`] - plain Rust helper that owns a fetched [`RowSet`] and a read
//!   position. Shared by `RsDataBasePrepare` (its own result) and `RsDataBaseCursor`.
//! * `RsDataBaseCursor` - PHP class returned in an `OUT` bind of type
//!   `RsDataBasePrepare::CURSOR` (Oracle `REF CURSOR`; PostgreSQL `refcursor` is stage 2).
//!
//! # Example (PHP)
//!
//! ```php
//! $cursor = null;
//! $q->createBind()->setName('rc')->setValue($cursor)->setType(RsDataBasePrepare::CURSOR);
//! $q->execute();
//! while (($row = $cursor->fetchRow()) !== null) { /* ... */ }
//! $cursor->free();
//! ```

use crate::driver::RowSet;
use crate::php::connect::ConnState;
use crate::php::convert::{guard, row_to_zval, rows_to_zval, PhpResult};
use crate::php::exception::{connection_error, usage_error};
use crate::php::request::StateRef;
use ext_php_rs::flags::ClassFlags;
use ext_php_rs::prelude::*;
use ext_php_rs::types::Zval;

/// A fully fetched result set plus a read position.
#[derive(Default)]
pub struct RowCursor {
    rowset: Option<RowSet>,
    pos: usize,
}

impl RowCursor {
    /// Replace the held result (resets the position).
    ///
    /// # Example
    ///
    /// ```ignore
    /// cursor.set(Some(rowset));
    /// ```
    pub fn set(&mut self, rowset: Option<RowSet>) {
        self.rowset = rowset;
        self.pos = 0;
    }

    /// Drop the held result.
    pub fn clear(&mut self) {
        self.set(None);
    }

    /// Number of columns of the held result (0 if none).
    pub fn column_count(&self) -> usize {
        self.rowset.as_ref().map(|r| r.columns.len()).unwrap_or(0)
    }

    /// Next row as an associative array, `None` after the last row.
    ///
    /// # Example
    ///
    /// ```ignore
    /// while let Some(row) = cursor.fetch_row() { /* row: PHP array zval */ }
    /// ```
    pub fn fetch_row(&mut self) -> Option<Zval> {
        let rs = self.rowset.as_ref()?;
        let row = rs.rows.get(self.pos)?;
        self.pos += 1;
        Some(row_to_zval(&rs.columns, row))
    }

    /// Up to `count` next rows as a PHP list of associative arrays.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let batch = cursor.fetch_rows(100);
    /// ```
    pub fn fetch_rows(&mut self, count: usize) -> Zval {
        let Some(rs) = self.rowset.as_ref() else {
            return rows_to_zval(&[], &[]);
        };
        let end = self.pos.saturating_add(count).min(rs.rows.len());
        let z = rows_to_zval(&rs.columns, &rs.rows[self.pos.min(end)..end]);
        self.pos = end;
        z
    }

    /// All remaining rows.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let all = cursor.fetch_all();
    /// ```
    pub fn fetch_all(&mut self) -> Zval {
        self.fetch_rows(usize::MAX)
    }
}

/// PHP class `RsDataBaseCursor` (final). Created by the extension only.
#[php_class]
#[php(name = "RsDataBaseCursor")]
#[php(flags = ClassFlags::Final)]
pub struct RsDataBaseCursor {
    cursor: RowCursor,
    state: StateRef,
    epoch: u64,
    freed: bool,
}

impl RsDataBaseCursor {
    /// Build a cursor bound to the current lease epoch of `state`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let c = RsDataBaseCursor::new_from_rowset(state.clone(), rowset);
    /// ```
    pub fn new_from_rowset(state: StateRef, rowset: RowSet) -> Self {
        let epoch = state.borrow().epoch;
        let mut cursor = RowCursor::default();
        cursor.set(Some(rowset));
        Self {
            cursor,
            state,
            epoch,
            freed: false,
        }
    }

    fn check(&self) -> PhpResult<()> {
        if self.freed {
            return Err(usage_error("cursor was freed"));
        }
        let s: std::cell::Ref<'_, ConnState> = self.state.borrow();
        if s.closed || s.epoch != self.epoch {
            return Err(connection_error(
                "cursor is no longer valid: the connection was closed",
            ));
        }
        Ok(())
    }
}

#[php_impl]
impl RsDataBaseCursor {
    /// Private constructor: cursors are created by `execute()`.
    ///
    /// ```php
    /// new RsDataBaseCursor(); // Error: Call to private ...
    /// ```
    #[php(vis = "private")]
    pub fn __construct() -> PhpResult<Self> {
        Err(usage_error(
            "RsDataBaseCursor can only be obtained from an OUT CURSOR bind",
        ))
    }

    /// Next row as an associative array, `null` after the last row.
    ///
    /// ```php
    /// $row = $cursor->fetchRow(); // ['ID' => 1, 'NAME' => 'x'] | null
    /// ```
    pub fn fetch_row(&mut self) -> PhpResult<Option<Zval>> {
        guard(|| {
            self.check()?;
            Ok(self.cursor.fetch_row())
        })
    }

    /// Up to `$count` next rows.
    ///
    /// ```php
    /// $batch = $cursor->fetchRows(100);
    /// ```
    pub fn fetch_rows(&mut self, count: i64) -> PhpResult<Zval> {
        guard(|| {
            self.check()?;
            if count < 0 {
                return Err(usage_error("fetchRows(): count must be >= 0"));
            }
            Ok(self.cursor.fetch_rows(count as usize))
        })
    }

    /// All remaining rows.
    ///
    /// ```php
    /// $rows = $cursor->fetchAllRows();
    /// ```
    pub fn fetch_all_rows(&mut self) -> PhpResult<Zval> {
        guard(|| {
            self.check()?;
            Ok(self.cursor.fetch_all())
        })
    }

    /// Number of columns.
    ///
    /// ```php
    /// echo $cursor->getColumnCount();
    /// ```
    pub fn get_column_count(&self) -> PhpResult<i64> {
        guard(|| {
            self.check()?;
            Ok(self.cursor.column_count() as i64)
        })
    }

    /// Release the cursor; further use throws.
    ///
    /// ```php
    /// $cursor->free();
    /// ```
    pub fn free(&mut self) -> PhpResult<()> {
        guard(|| {
            self.freed = true;
            self.cursor.clear();
            Ok(())
        })
    }
}
