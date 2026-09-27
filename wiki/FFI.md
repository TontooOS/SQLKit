# FFI

C header and memory rules for embedding SQLKit.

## Functions

| Function | Return | Meaning |
|---|---|---|
| `sqlkit_open` | `SQLKitConnection *` | File-backed handle, or `NULL` on error |
| `sqlkit_open_memory` | `SQLKitConnection *` | In-memory handle, or `NULL` on error |
| `sqlkit_exec` | `int64_t` | Rows changed, or negative on error |
| `sqlkit_exec_batch` | `int` | `0` on success, negative on error |
| `sqlkit_close` | `void` | Releases the handle |
| `sqlkit_version` | `const char *` | Version string, do NOT free |
| `sqlkit_free_string` | `void` | Frees strings returned by SQLKit |

## Memory Rules

Handles are opaque `SQLKitConnection *` pointers created by `sqlkit_open` or `sqlkit_open_memory` and released exactly once with `sqlkit_close`. Passing `NULL` to `sqlkit_close` is safe. Version strings are static and must never be freed.

## Usage / Example

```c
SQLKitConnection *db = sqlkit_open_memory();
int rc = sqlkit_exec_batch(db, "CREATE TABLE t (id TEXT PRIMARY KEY)");
sqlkit_close(db);
```

## Cross References

- [Connection.md](Connection.md) – Rust behavior behind the C calls
- [Pager.md](Pager.md) – file behavior for `sqlkit_open`
