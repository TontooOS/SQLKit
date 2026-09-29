# Tontoo SQLKit

Dependency-light SQLite-compatible engine for TontooOS. Exposes a `rusqlite`-compatible subset (`Connection`, `Statement`, `params!`, `Row`, `Transaction`) over a pure-Rust engine with streaming iterators and atomic commits. Built so CoreData and other frameworks can drop `rusqlite` / `libsqlite3` without changing call sites.

Status: production backend for CoreData (`SqliteStore`). `CREATE TABLE` / `CREATE INDEX`, `INSERT` (`OR REPLACE` / `OR IGNORE`), `SELECT` with `WHERE` filters, `JOIN`, sub-selects, aggregates, `GROUP BY` / `HAVING`, `DISTINCT`, `UNION`, `ORDER BY`, `LIMIT`, `UPDATE`, `DELETE`, `PRAGMA`, transactions, and native B-Tree file persistence work (files are byte-level real SQLite, verified with rusqlite, the SQLite CLI, and CPython `sqlite3`). Triggers, views, and exotic indexes are roadmap items.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["SQLKit"] }
```

## License

TCL v27.0
