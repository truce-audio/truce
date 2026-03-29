#pragma once

#include "AAX_CEffectGUI.h"
#include "TruceAAX_Bridge.h"

class TruceAAX_GUI : public AAX_CEffectGUI {
public:
    TruceAAX_GUI();
    ~TruceAAX_GUI() override;

    static AAX_IEffectGUI* AAX_CALLBACK Create();

    void CreateViewContents() override;
    void CreateViewContainer() override;
    void DeleteViewContainer() override;

    AAX_Result GetViewSize(AAX_Point* oViewSize) const override;
    AAX_Result TimerWakeup() override;
    AAX_Result ParameterUpdated(AAX_CParamID paramID) override;

    void* GetRustCtx();

private:
    bool mEditorExists;
    bool mViewOpen;
    TruceAaxGuiCallbacks mCallbacks;

    // Only one editor view can be open at a time (wgpu/Metal limitation).
    static TruceAAX_GUI* sOpenInstance;

    static void CB_TouchParam(void* aax_ctx, uint32_t param_id);
    static void CB_SetParam(void* aax_ctx, uint32_t param_id, double normalized);
    static void CB_ReleaseParam(void* aax_ctx, uint32_t param_id);
    static int  CB_RequestResize(void* aax_ctx, uint32_t w, uint32_t h);
};
