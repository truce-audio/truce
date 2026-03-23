/**
 * Bridge loader — dlopen()s the Rust cdylib and resolves symbols.
 */

#include "TruceAAX_Bridge.h"
#include <cstdio>
#include <cstring>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#include <dirent.h>
#endif

TruceBridge g_bridge = {};
TruceAaxDescriptor g_descriptor = {};
bool g_bridge_loaded = false;

#ifdef _WIN32
static void* load_lib(const char* path) { return (void*)LoadLibraryA(path); }
static void* get_sym(void* lib, const char* name) { return (void*)GetProcAddress((HMODULE)lib, name); }
static void  close_lib(void* lib) { FreeLibrary((HMODULE)lib); }
#else
static void* load_lib(const char* path) { return dlopen(path, RTLD_NOW | RTLD_LOCAL); }
static void* get_sym(void* lib, const char* name) { return dlsym(lib, name); }
static void  close_lib(void* lib) { dlclose(lib); }
#endif

// Find the first .dylib/.dll in the Resources directory
static bool find_rust_dylib(const char* bundle_path, char* out, size_t out_len) {
    char resources[1024];
    snprintf(resources, sizeof(resources), "%s/Contents/Resources", bundle_path);

#ifdef _WIN32
    char pattern[1024];
    snprintf(pattern, sizeof(pattern), "%s\\*.dll", resources);
    WIN32_FIND_DATAA fd;
    HANDLE h = FindFirstFileA(pattern, &fd);
    if (h == INVALID_HANDLE_VALUE) return false;
    snprintf(out, out_len, "%s\\%s", resources, fd.cFileName);
    FindClose(h);
    return true;
#else
    DIR* dir = opendir(resources);
    if (!dir) return false;
    struct dirent* entry;
    while ((entry = readdir(dir)) != nullptr) {
        const char* name = entry->d_name;
        size_t len = strlen(name);
        if ((len > 6 && strcmp(name + len - 6, ".dylib") == 0) ||
            (len > 3 && strcmp(name + len - 3, ".so") == 0)) {
            snprintf(out, out_len, "%s/%s", resources, name);
            closedir(dir);
            return true;
        }
    }
    closedir(dir);
    return false;
#endif
}

bool TruceBridge_Load(TruceBridge* bridge, const char* bundle_path) {
    char dylib_path[2048];
    if (!find_rust_dylib(bundle_path, dylib_path, sizeof(dylib_path))) {
        fprintf(stderr, "[truce-aax] No Rust cdylib found in %s/Contents/Resources/\n", bundle_path);
        return false;
    }

    void* lib = load_lib(dylib_path);
    if (!lib) {
#ifndef _WIN32
        fprintf(stderr, "[truce-aax] dlopen failed: %s\n", dlerror());
#endif
        return false;
    }

    bridge->lib_handle = lib;

#define RESOLVE(name) \
    bridge->name = (fn_##name)get_sym(lib, "truce_aax_" #name); \
    if (!bridge->name) { \
        fprintf(stderr, "[truce-aax] Missing symbol: truce_aax_" #name "\n"); \
        close_lib(lib); \
        return false; \
    }

    RESOLVE(get_descriptor);
    RESOLVE(get_param_info);
    RESOLVE(create);
    RESOLVE(destroy);
    RESOLVE(reset);
    RESOLVE(process);
    RESOLVE(get_param);
    RESOLVE(set_param);
    RESOLVE(format_param);
    RESOLVE(save_state);
    RESOLVE(load_state);
    RESOLVE(free_state);
    RESOLVE(editor_create);
    RESOLVE(editor_open);
    RESOLVE(editor_close);
    RESOLVE(editor_idle);
    RESOLVE(editor_get_size);

#undef RESOLVE

    // Read the plugin descriptor
    bridge->get_descriptor(&g_descriptor);

    g_bridge_loaded = true;
    return true;
}

void TruceBridge_Unload(TruceBridge* bridge) {
    if (bridge->lib_handle) {
        close_lib(bridge->lib_handle);
        bridge->lib_handle = nullptr;
    }
    g_bridge_loaded = false;
}
