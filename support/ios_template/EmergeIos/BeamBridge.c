#include "BeamBridge.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>
#include <sys/stat.h>

extern int erl_start(int argc, char *argv[]);

static bool g_initialized = false;

#define MAX_ARGS 256

/// Find the ERTS version directory under `erlang/erts-*`.
/// Returns a newly allocated string (caller must free) or NULL.
static char* find_erts_bin(const char* erl_root) {
    char erts_dir[1024];
    DIR* dir = opendir(erl_root);
    if (!dir) return NULL;

    struct dirent* entry;
    char* result = NULL;
    while ((entry = readdir(dir)) != NULL) {
        if (strncmp(entry->d_name, "erts-", 5) == 0) {
            snprintf(erts_dir, sizeof(erts_dir), "%s/%s/bin", erl_root, entry->d_name);
            struct stat st;
            if (stat(erts_dir, &st) == 0 && S_ISDIR(st.st_mode)) {
                result = strdup(erts_dir);
                break;
            }
        }
    }
    closedir(dir);
    return result;
}

/// Find the highest-numbered release directory under `releases/`.
/// Returns a newly allocated string (caller must free) or NULL.
static char* find_latest_release(const char* erl_root) {
    char releases_path[1024];
    snprintf(releases_path, sizeof(releases_path), "%s/releases", erl_root);

    DIR* dir = opendir(releases_path);
    if (!dir) return NULL;

    struct dirent* entry;
    int best_ver = 0;
    char best_dir[1024] = {0};

    while ((entry = readdir(dir)) != NULL) {
        if (entry->d_type != DT_DIR) continue;
        int ver = atoi(entry->d_name);
        if (ver > 0 && ver > best_ver) {
            best_ver = ver;
            snprintf(best_dir, sizeof(best_dir), "%s/releases/%s", erl_root, entry->d_name);
        }
    }
    closedir(dir);

    if (best_ver > 0) {
        return strdup(best_dir);
    }
    return NULL;
}

/// Scan `lib/` for all `<app>/ebin/` directories and add them to `paths`.
/// Returns the number of paths added.
static int collect_pa_paths(const char* lib_path, char* paths[], int max) {
    DIR* dir = opendir(lib_path);
    if (!dir) return 0;

    int count = 0;
    struct dirent* entry;
    while ((entry = readdir(dir)) != NULL && count < max) {
        if (entry->d_type != DT_DIR) continue;
        if (entry->d_name[0] == '.') continue;

        char ebin[2048];
        snprintf(ebin, sizeof(ebin), "%s/%s/ebin", lib_path, entry->d_name);

        struct stat st;
        if (stat(ebin, &st) == 0 && S_ISDIR(st.st_mode)) {
            paths[count] = strdup(ebin);
            count++;
        }
    }
    closedir(dir);
    return count;
}

int beam_init(const char* erl_root, const char* eval_expr) {
    if (g_initialized) return 0;
    if (!erl_root) return -1;

    // OTP release layout (bundled as "erlang/" in the app):
    //   erlang/
    //     bin/              — boot scripts and Erlang tools
    //     erts-<vsn>/bin/   — beam.smp and other native binaries
    //     lib/              — OTP + Elixir + user app libraries
    //     releases/<vsn>/   — boot scripts, .rel files

    // Set BINDIR to the ERTS bin directory (where beam.smp lives)
    char* erts_bin = find_erts_bin(erl_root);
    if (!erts_bin) {
        fprintf(stderr, "beam_init: could not find erts-*/bin under %s\n", erl_root);
        return -1;
    }
    char* bindir_argv = strdup(erts_bin);
    setenv("BINDIR", erts_bin, 1);
    free(erts_bin);

    setenv("ROOTDIR", erl_root, 1);
    setenv("EMU", "beam.smp", 1);

    // Find the latest release directory for boot script
    char* release_dir = find_latest_release(erl_root);
    if (!release_dir) {
        fprintf(stderr, "beam_init: could not find releases/ dir under %s\n", erl_root);
        return -1;
    }

    char boot_path[1024];
    snprintf(boot_path, sizeof(boot_path), "%s/start_clean.boot", release_dir);
    char boot_prefix[1024];
    snprintf(boot_prefix, sizeof(boot_prefix), "%s/start_clean", release_dir);
    free(release_dir);

    struct stat st;
    if (stat(boot_path, &st) != 0) {
        fprintf(stderr, "beam_init: boot script not found: %s\n", boot_path);
        return -1;
    }

    // Collect -pa paths from lib/ (covers OTP apps + Elixir + user apps)
    char lib_path[1024];
    snprintf(lib_path, sizeof(lib_path), "%s/lib", erl_root);

    char* pa_paths[MAX_ARGS];
    int pa_count = 0;

    struct stat lib_st;
    if (stat(lib_path, &lib_st) == 0 && S_ISDIR(lib_st.st_mode)) {
        pa_count = collect_pa_paths(lib_path, pa_paths, MAX_ARGS - 20);
    }

    // Build the args array for erl_start
    char* args[MAX_ARGS];
    int argc = 0;

    args[argc++] = "beam.smp";
    args[argc++] = "--";
    args[argc++] = "-sbwt";
    args[argc++] = "none";
    args[argc++] = "-noshell";
    args[argc++] = "-root";
    args[argc++] = (char*)erl_root;
    args[argc++] = "-bindir";
    args[argc++] = bindir_argv;
    args[argc++] = "-setcookie";
    args[argc++] = "emerge_ios";
    const char* home = getenv("HOME");
    if (home) {
        args[argc++] = "-home";
        args[argc++] = (char*)home;
    }
    args[argc++] = "-no_epmd";
    args[argc++] = "-dist_listen";
    args[argc++] = "false";
    args[argc++] = "-boot";
    args[argc++] = boot_prefix;

    // Add -pa for each ebin directory found in lib/
    for (int i = 0; i < pa_count; i++) {
        args[argc++] = "-pa";
        args[argc++] = pa_paths[i];
    }

    // Add -eval if provided — starts Elixir runtime and user app
    if (eval_expr && strlen(eval_expr) > 0) {
        args[argc++] = "-eval";
        args[argc++] = (char*)eval_expr;
    }

    args[argc] = NULL;

    fprintf(stderr, "beam_init: %d args, root=%s\n", argc, erl_root);
    for (int i = 0; i < argc; i++) {
        fprintf(stderr, "  [%d] %s\n", i, args[i]);
    }

    int result = erl_start(argc, args);

    for (int i = 0; i < pa_count; i++) {
        free(pa_paths[i]);
    }

    if (result == 0) g_initialized = true;
    return result;
}

bool beam_is_initialized(void) {
    return g_initialized;
}
