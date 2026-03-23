#include "TruceAAX_GUI.h"
#include "TruceAAX_Parameters.h"

#include <cstring>
#include <cstdio>
#include <sstream>

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

AAX_IEffectGUI* AAX_CALLBACK TruceAAX_GUI::Create() {
    return new TruceAAX_GUI();
}

// ---------------------------------------------------------------------------
// Constructor / Destructor
// ---------------------------------------------------------------------------

TruceAAX_GUI::TruceAAX_GUI()
    : AAX_CEffectGUI()
    , mEditorExists(false)
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

    void* parentView = GetViewContainerPtr();
    int platform = (int)GetViewContainerType();

    // Set up callbacks so Rust editor can notify AAX of param gestures
    mCallbacks.aax_ctx = this;
    mCallbacks.touch_param = CB_TouchParam;
    mCallbacks.set_param = CB_SetParam;
    mCallbacks.release_param = CB_ReleaseParam;
    mCallbacks.request_resize = CB_RequestResize;

    g_bridge.editor_open(ctx, parentView, platform, &mCallbacks);
}

void TruceAAX_GUI::DeleteViewContainer() {
    void* ctx = GetRustCtx();
    if (ctx && g_bridge_loaded && g_bridge.editor_close) {
        g_bridge.editor_close(ctx);
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
