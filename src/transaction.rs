//! `Transaction`: RAII transaction handle with commit / rollback.

use crate::connection::Connection;
use crate::error::Result;
use crate::statement::{Row, Statement};
use crate::value::IntoParams;

/// A database transaction. Rolls back on drop unless `commit` was called.
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    committed: bool,
}

impl<'conn> Transaction<'conn> {
    pub(crate) fn new(conn: &'conn Connection) -> Self {
        Self {
            conn,
            committed: false,
        }
    }

    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        self.conn.execute(sql, params)
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.conn.prepare(sql)
    }

    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: IntoParams,
        F: FnOnce(&Row) -> Result<T>,
    {
        self.conn.query_row(sql, params, f)
    }

    pub fn commit(mut self) -> Result<()> {
        self.conn.commit_inner()?;
        self.committed = true;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<()> {
        self.conn.rollback_inner()?;
        self.committed = true;
        Ok(())
    }
}

impl<'conn> Drop for Transaction<'conn> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.rollback_inner();
        }
    }
}
