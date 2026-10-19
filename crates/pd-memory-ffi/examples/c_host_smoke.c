/*
 * c_host_smoke.c — minimal C host integration test for the pd-memory SDK.
 *
 * Build & run from `sdk/pd-memory/` (macOS / Linux):
 *
 *   cargo build -p pd-memory-ffi
 *   cc -Wall -Wextra -I crates/pd-memory-ffi/include \
 *       crates/pd-memory-ffi/examples/c_host_smoke.c \
 *       -L target/debug -lpd_memory_ffi \
 *       -Wl,-rpath,"$PWD/target/debug" \
 *       -o /tmp/pd_memory_smoke
 *   PD_ROOT="$(mktemp -d)" /tmp/pd_memory_smoke
 *
 * Every call returns a heap JSON envelope that must be freed with
 * pd_memory_string_free(); pd_memory_version() returns a static string.
 */

#include <stdio.h>
#include <stdlib.h>

#include "pd_memory.h"

int main(void) {
    printf("version=%s\n", pd_memory_version());

    char config[1024];
    snprintf(config, sizeof(config),
             "{\"data_root\":\"%s\",\"workspace_id\":\"c-smoke\"}",
             getenv("PD_ROOT"));

    PdMemoryHandle *handle = pd_memory_new(config);
    if (handle == NULL) {
        char *error = pd_memory_last_error(NULL); /* NULL: config/init error */
        fprintf(stderr, "pd_memory_new failed: %s\n", error);
        pd_memory_string_free(error);
        return 1;
    }

    char *out = pd_memory_call(handle, "overview", "{}");
    printf("overview=%s\n", out);
    pd_memory_string_free(out);

    out = pd_memory_call(
        handle, "add",
        "{\"messages\":[{\"role\":\"user\",\"content\":\"C host smoke test\"}],"
        "\"session_id\":\"c-session\",\"source\":\"c-smoke\"}");
    printf("add=%s\n", out);
    pd_memory_string_free(out);

    out = pd_memory_call(handle, "not_an_op", "{}");
    printf("invalid_op=%s\n", out);
    pd_memory_string_free(out);

    char *last = pd_memory_last_error(handle);
    printf("last_error_non_empty=%d\n", last != NULL && last[0] != '\0');
    pd_memory_string_free(last);

    pd_memory_free(handle);
    return 0;
}
