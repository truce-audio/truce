#include "TruceAAX_GUI.h"
#include "TruceAAX_Parameters.h"

#include <cstring>
#include <cstdio>
#include <sstream>

#ifdef __APPLE__
extern "C" void* objc_autoreleasePoolPush();
extern "C" void  objc_autoreleasePoolPop(void*);
#endif

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

AAX_IEffectGUI* AAX_CALLBACK TruceAAX_GUI::Create() {
    return new TruceAAX_GUI();
}

// ---------------------------------------------------------------------------
// Constructor / Destructor
// ---------------------------------------------------------------------------

TruceAAX_GUI* TruceAAX_GUI::sOpenInstance = nullptr;

TruceAAX_GUI::TruceAAX_GUI()
    : AAX_CEffectGUI()
    , mEditorExists(false)
    , mViewOpen(false)
    , mCallbacks{} {
}

TruceAAX_GUI::~TruceAAX_GUI() {
}

// ---------------------------------------------------------------------------
// Access Rust context from the parameters component
// ---------------------------------------------------------------------------

void* TruceAAX_GUI::GetRustCtx() {
    auto* params = dynamic_cast<TruceAAX_Parameters*>(GetEffectParameters());
    return params ? params->GetRustCtx() : nullptr;
}

// ---------------------------------------------------------------------------
// View lifecycle
// ---------------------------------------------------------------------------

void TruceAAX_GUI::CreateViewContents() {
    void* ctx = GetRustCtx();
    if (!ctx || !g_bridge_loaded || !g_bridge.editor_create) return;

    TruceAaxEditorInfo info = {};
    g_bridge.editor_create(ctx, &info);
    mEditorExists = (info.has_editor != 0);
}

void TruceAAX_GUI::CreateViewContainer() {
    if (!mEditorExists) return;

    void* ctx = GetRustCtx();
    if (!ctx || !g_bridge.editor_open) return;

    // Close any previously open editor.  baseview's NSView teardown
    // autoreleases ObjC objects (CAMetalLayer, MTLDevice refs, etc.).
    // Wrapping the close in an explicit autorelease pool flushes those
    // immediately, preventing a use-after-free when the outer pool drains
    // during the next CFRunLoop timer tick.
    if (sOpenInstance && sOpenInstance != this && sOpenInstance->mViewOpen) {
#ifdef __APPLE__
        void* pool = objc_autoreleasePoolPush();
#endif
        void* prevCtx = sOpenInstance->GetRustCtx();
        if (prevCtx && g_bridge.editor_close) {
            g_bridge.editor_close(prevCtx);
        }
        sOpenInstance->mViewOpen = false;
#ifdef __APPLE__
        objc_autoreleasePoolPop(pool);
#endif
    }

    void* parentView = GetViewContainerPtr();
    int platform = (int)GetViewContainerType();

    // Set up callbacks so Rust editor can notify AAX of param gestures
    mCallbacks.aax_ctx = this;
    mCallbacks.touch_param = CB_TouchParam;
    mCallbacks.set_param = CB_SetParam;
    mCallbacks.release_param = CB_ReleaseParam;
    mCallbacks.request_resize = CB_RequestResize;

    g_bridge.editor_open(ctx, parentView, platform, &mCallbacks);
    mViewOpen = true;
    sOpenInstance = this;
}

void TruceAAX_GUI::DeleteViewContainer() {
    if (!mViewOpen) return;
    void* ctx = GetRustCtx();
    if (ctx && g_bridge_loaded && g_bridge.editor_close) {
#ifdef __APPLE__
        void* pool = objc_autoreleasePoolPush();
        g_bridge.editor_close(ctx);
        objc_autoreleasePoolPop(pool);
#else
        g_bridge.editor_close(ctx);
#endif
    }
    mViewOpen = false;
    if (sOpenInstance == this) {
        sOpenInstance = nullptr;
    }
}

// ---------------------------------------------------------------------------
// Size, idle, param updates
// ---------------------------------------------------------------------------

AAX_Result TruceAAX_GUI::GetViewSize(AAX_Point* oViewSize) const {
    void* ctx = const_cast<TruceAAX_GUI*>(this)->GetRustCtx();
    if (!ctx || !mEditorExists || !g_bridge.editor_get_size)
        return AAX_ERROR_NULL_OBJECT;

    uint32_t w = 0, h = 0;
    if (g_bridge.editor_get_size(ctx, &w, &h)) {
        oViewSize->horz = (float)w;
        oViewSize->vert = (float)h;
        return AAX_SUCCESS;
    }
    return AAX_ERROR_NULL_OBJECT;
}

AAX_Result TruceAAX_GUI::TimerWakeup() {
    if (!mViewOpen) return AAX_SUCCESS;
    void* ctx = GetRustCtx();
    if (ctx && mEditorExists && g_bridge_loaded && g_bridge.editor_idle) {
        g_bridge.editor_idle(ctx);
    }
    return AAX_SUCCESS;
}

AAX_Result TruceAAX_GUI::ParameterUpdated(AAX_CParamID paramID) {
    // GUI picks up param changes via get_param on each render tick.
    // No action needed here.
    return AAX_SUCCESS;
}

// ---------------------------------------------------------------------------
// Callbacks from Rust → AAX (parameter gestures)
// ---------------------------------------------------------------------------

void TruceAAX_GUI::CB_TouchParam(void* aax_ctx, uint32_t param_id) {
    auto* gui = static_cast<TruceAAX_GUI*>(aax_ctx);
    std::ostringstream idStr;
    idStr << "truce_p" << param_id;
    gui->GetEffectParameters()->TouchParameter(idStr.str().c_str());
}

void TruceAAX_GUI::CB_SetParam(void* aax_ctx, uint32_t param_id, double normalized) {
    auto* gui = static_cast<TruceAAX_GUI*>(aax_ctx);
    std::ostringstream idStr;
    idStr << "truce_p" << param_id;
    gui->GetEffectParameters()->SetParameterNormalizedValue(
        idStr.str().c_str(), normalized);
}

void TruceAAX_GUI::CB_ReleaseParam(void* aax_ctx, uint32_t param_id) {
    auto* gui = static_cast<TruceAAX_GUI*>(aax_ctx);
    std::ostringstream idStr;
    idStr << "truce_p" << param_id;
    gui->GetEffectParameters()->ReleaseParameter(idStr.str().c_str());
}

int TruceAAX_GUI::CB_RequestResize(void* aax_ctx, uint32_t w, uint32_t h) {
    // AAX resize is complex; skip for now
    return 0;
}
