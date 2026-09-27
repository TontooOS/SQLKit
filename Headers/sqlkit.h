/*
 * Tontoo SQLKit - C Header
 * TontooOS SQLite-compatible engine
 *
 * Basis scope: open / execute / execute_batch / close / version.
 */

#ifndef TONTOO_SQLKIT_H
#define TONTOO_SQLKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque connection handle (sqlkit::Connection). */
typedef struct SQLKitConnection SQLKitConnection;

/**
 * Open a file-backed database.
 *
 * @param path Filesystem path to the .sqlite file
 * @return Opaque handle, or NULL on error
 */
SQLKitConnection *sqlkit_open(const char *path);

/**
 * Open a transient in-memory database.
 *
 * @return Opaque handle, or NULL on error
 */
SQLKitConnection *sqlkit_open_memory(void);

/**
 * Execute a single write statement without parameters.
 *
 * @param db Connection handle
 * @param sql Single SQL statement
 * @return Rows changed, or negative on error
 */
int64_t sqlkit_exec(SQLKitConnection *db, const char *sql);

/**
 * Execute a batch of statements without parameters.
 *
 * @param db Connection handle
 * @param sql One or more SQL statements
 * @return 0 on success, negative on error
 */
int sqlkit_exec_batch(SQLKitConnection *db, const char *sql);

/**
 * Close a database handle.
 *
 * @param db Handle from sqlkit_open / sqlkit_open_memory (or NULL)
 */
void sqlkit_close(SQLKitConnection *db);

/**
 * Get the library version string.
 *
 * @return Version string (do NOT free)
 */
const char *sqlkit_version(void);

/**
 * Free a string previously returned by a sqlkit_* function.
 *
 * @param s String to free
 */
void sqlkit_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* TONTOO_SQLKIT_H */
