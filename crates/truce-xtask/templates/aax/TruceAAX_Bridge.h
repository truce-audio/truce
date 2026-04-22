/**
 * Bridge loader — dlopen()s the Rust cdylib and resolves truce_aax_* symbols.
 */

#pragma once

#include "truce_aax_bridge.h"

// Function pointer types matching the Rust exports
typedef void  (*fn_get_descriptor)(TruceAaxDescriptor*);
typedef void  (*fn_get_param_info)(uint32_t, TruceAaxParamInfo*);
typedef void* (*fn_create)(void);
typedef void  (*fn_destroy)(void*);
typedef void  (*fn_reset)(void*, double, uint32_t);
typedef void  (*fn_process)(void*, const float**, float**, uint32_t, uint32_t, uint32_t,
                            const TruceAaxMidiEvent*, uint32_t,
                            const TruceAaxTransportSnapshot*);
typedef double (*fn_get_param)(void*, uint32_t);
typedef void   (*fn_set_param)(void*, uint32_t, double);
typedef void   (*fn_format_param)(void*, uint32_t, double, char*, uint32_t);
typedef uint32_t (*fn_save_state)(void*, uint8_t**);
typedef void     (*fn_load_state)(void*, const uint8_t*, uint32_t);
typedef void     (*fn_free_state)(uint8_t*, uint32_t);

// GUI
typedef void     (*fn_editor_create)(void*, TruceAaxEditorInfo*);
typedef void     (*fn_editor_open)(void*, void*, int, const TruceAaxGuiCallbacks*);
typedef void     (*fn_editor_close)(void*);
typedef void     (*fn_editor_idle)(void*);
typedef int      (*fn_editor_get_size)(void*, uint32_t*, uint32_t*);

struct TruceBridge {
    fn_get_descriptor get_descriptor;
    fn_get_param_info get_param_info;
    fn_create         create;
    fn_destroy        destroy;
    fn_reset          reset;
    fn_process        process;
    fn_get_param      get_param;
    fn_set_param      set_param;
    fn_format_param   format_param;
    fn_save_state     save_state;
    fn_load_state     load_state;
    fn_free_state     free_state;
    fn_editor_create  editor_create;
    fn_editor_open    editor_open;
    fn_editor_close   editor_close;
    fn_editor_idle    editor_idle;
    fn_editor_get_size editor_get_size;
    void*             lib_handle;
};

// Load the Rust cdylib from the bundle's Resources directory.
// Returns true on success.
bool TruceBridge_Load(TruceBridge* bridge, const char* bundle_path);
void TruceBridge_Unload(TruceBridge* bridge);

// Global bridge instance (loaded once)
extern TruceBridge g_bridge;
extern TruceAaxDescriptor g_descriptor;
extern bool g_bridge_loaded;
