cmake_minimum_required(VERSION 3.19)
project(TruceAAXTemplate)

# AAX SDK path
if(NOT DEFINED AAX_SDK_PATH)
    if(DEFINED ENV{AAX_SDK_PATH})
        set(AAX_SDK_PATH $ENV{AAX_SDK_PATH})
    else()
        message(FATAL_ERROR "Set AAX_SDK_PATH to the AAX SDK root directory.")
    endif()
endif()

# AAX library — use pre-built if available, otherwise build
set(AAX_LIB_PATH "${AAX_SDK_PATH}/build-lib/Libs/AAXLibrary/libAAXLibrary.a")
if(NOT EXISTS "${AAX_LIB_PATH}")
    message(STATUS "Pre-built AAX library not found, building from source...")
    add_subdirectory(${AAX_SDK_PATH}/Libs/AAXLibrary ${CMAKE_BINARY_DIR}/AAXLibrary)
    set(AAX_LIB_TARGET AAXLibrary)
else()
    message(STATUS "Using pre-built AAX library: ${AAX_LIB_PATH}")
    add_library(AAXLibrary STATIC IMPORTED)
    set_target_properties(AAXLibrary PROPERTIES IMPORTED_LOCATION "${AAX_LIB_PATH}")
    set(AAX_LIB_TARGET AAXLibrary)
endif()

# Template plugin
add_library(TruceAAXTemplate MODULE
    src/TruceAAX_Describe.cpp
    src/TruceAAX_Parameters.cpp
    src/TruceAAX_Bridge.cpp
    src/TruceAAX_GUI.cpp
    ${AAX_SDK_PATH}/Extensions/Topology/AAX_CMonolithicParameters.cpp
    ${AAX_SDK_PATH}/Interfaces/AAX_Exports.cpp
)

target_include_directories(TruceAAXTemplate PRIVATE
    ${AAX_SDK_PATH}/Interfaces
    ${AAX_SDK_PATH}/Interfaces/ACF
    ${AAX_SDK_PATH}/Extensions/Topology
    ${AAX_SDK_PATH}/Extensions/GUI
    ${CMAKE_CURRENT_SOURCE_DIR}/src
)

target_link_libraries(TruceAAXTemplate PRIVATE ${AAX_LIB_TARGET})

# macOS: bundle + frameworks
if(APPLE)
    target_link_libraries(TruceAAXTemplate PRIVATE "-framework CoreFoundation" "-framework Cocoa")
    set_target_properties(TruceAAXTemplate PROPERTIES
        BUNDLE TRUE
        BUNDLE_EXTENSION "aaxplugin"
        MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/src/Info.plist.in
    )
endif()

target_compile_features(TruceAAXTemplate PRIVATE cxx_std_17)
