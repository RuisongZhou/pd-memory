/*
 * pd_memory.h — stable C ABI for the pd-memory SDK.
 *
 * One opaque handle per memory workspace/engine:
 *
 *   #include "pd_memory.h"
 *
 *   PdMemoryHandle *h = pd_memory_new(
 *       "{\"data_root\":\"/var/lib/pd-memory\",\"workspace_id\":\"my-project\"}");
 *   if (h == NULL) {                 // config / engine-init error
 *       char *err = pd_memory_last_error(NULL);   // {"ok":false,...}
 *       fprintf(stderr, "%s\n", err);
 *       pd_memory_string_free(err);
 *       return 1;
 *   }
 *
 *   char *out = pd_memory_call(h, "add",
 *       "{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}],"
 *        "\"session_id\":\"s1\",\"source\":\"my-host\"}");
 *   // out == {"ok":true,"result":{"captured":true,...}}
 *   puts(out);
 *   pd_memory_string_free(out);
 *
 *   pd_memory_free(h);
 *
 * Envelope: every call returns JSON
 *   {"ok":true,"result":<op result>}
 *   {"ok":false,"code":"<stable code>","message":"<human readable>"}
 *
 * Stable error codes: invalid_config, io, corrupt_data, model, model_output,
 * json, path_escape, not_found, unsupported, locked, invalid_input, other,
 * unknown_op, null_pointer, null_handle, panic.
 *
 * Operations (op names are case- and separator-insensitive):
 *   add (alias capture_turn)
 *       args  {messages:[{role,content}], session_id?, source?, timestamp?}
 *       result CaptureTurnResult {captured, normalized_messages, session_key}
 *   retrieve | search
 *       args  {query, recent_messages?, workspace_hint?, explicit?}
 *       result RetrievalResult {route, context?, system_context?, selected_ids,
 *                               intent?, cache_hit, trace}
 *   list            args {kinds?, query?, include_deprecated?, limit?, offset?}
 *                   result [MemoryManifestEntry]
 *   get             args {ids:[string], max_lines?}   result [MemoryFileRecord]
 *   overview        args {}                           result MemoryOverview
 *   flush           args {batch_size?, session_keys?, reason?} -> HeartbeatStats
 *   dream           args {trigger?: "manual"|"scheduled"}      -> DreamRunResult
 *   rollback        args {}                           -> DreamRollbackResult
 *   export          args {}                           -> MemoryExportBundle
 *   import          args {"bundle": <MemoryExportBundle>} -> MemoryImportResult
 *   settings_get    args {}                           -> IndexingSettings
 *   settings_save   args {"settings": <IndexingSettings>} -> IndexingSettings
 *   act             args {action: "edit_project_meta"|"edit_entry"|...}
 *                   result MemoryActionResult
 *   state_get       args {key}                        -> JSON value or null
 *   memory_context  args {query, recent_messages?}    -> {"systemContext": ...}
 *
 * Threading: the engine is blocking (it does file I/O and may call a model over
 * the network). Call from a worker thread; the handle itself may be shared by
 * several threads (engine internals are synchronised), but pd_memory_free must
 * not race with any other call on the same handle.
 *
 * Memory ownership:
 *   pd_memory_version()      -> static string, do NOT free.
 *   pd_memory_call()         -> heap string, free with pd_memory_string_free().
 *   pd_memory_last_error()   -> heap string, free with pd_memory_string_free().
 *   pd_memory_string_free(NULL) and pd_memory_free(NULL) are no-ops.
 */

#ifndef PD_MEMORY_H
#define PD_MEMORY_H

#ifdef __cplusplus
extern "C" {
#endif

/** Opaque engine handle; create with pd_memory_new(), destroy with pd_memory_free(). */
typedef struct PdMemoryHandle PdMemoryHandle;

/** Library version (static, NUL-terminated; do not free). */
const char *pd_memory_version(void);

/**
 * Create an engine from a JSON config (see pd-memory-core MemoryConfig:
 * data_root, workspace_id, workspace_root, model, retrieval, redaction, ...).
 * Returns NULL on failure; use pd_memory_last_error(NULL) to read the envelope.
 */
PdMemoryHandle *pd_memory_new(const char *config_json);

/** Destroy a handle. Must be called exactly once; NULL is a no-op. */
void pd_memory_free(PdMemoryHandle *handle);

/**
 * Run one operation. `args_json` may be NULL or "" (meaning {}). Returns a
 * heap-allocated JSON envelope; never NULL on success. Free with
 * pd_memory_string_free().
 */
char *pd_memory_call(PdMemoryHandle *handle, const char *op, const char *args_json);

/**
 * Last error envelope for `handle`, or the thread-local error for a NULL
 * handle (e.g. after pd_memory_new returned NULL). Empty string if none.
 * Heap-allocated; free with pd_memory_string_free().
 */
char *pd_memory_last_error(PdMemoryHandle *handle);

/** Free a string returned by pd_memory_call() or pd_memory_last_error(). */
void pd_memory_string_free(char *value);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* PD_MEMORY_H */
