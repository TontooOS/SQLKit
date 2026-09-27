# Tontoo SQLKit

Dependency-light SQLite-compatible engine for TontooOS. Exposes a `rusqlite`-compatible subset (`Connection`, `Statement`, `params!`, `Row`, `Transaction`) over a pure-Rust engine with streaming iterators and atomic commits. Built so CoreData and other frameworks can drop `rusqlite` / `libsqlite3` without changing call sites.

Status: basis engine (not yet used anywhere). `CREATE TABLE` / `CREATE INDEX`, `INSERT` (`OR REPLACE` / `OR IGNORE`), `SELECT` with `WHERE col = ?` filters, `UPDATE`, `DELETE`, `PRAGMA`, transactions, and native B-Tree file persistence work (files are byte-level real SQLite, verified with rusqlite, the SQLite CLI, and CPython `sqlite3`). JOIN, sub-selects, triggers, views, and exotic indexes are roadmap items.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["SQLKit"] }
```

## License

TCL v26.1
