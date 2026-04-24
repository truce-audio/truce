cmake_minimum_required(VERSION 3.19)
if(APPLE)
    project(TruceAAXTemplate LANGUAGES CXX OBJCXX)
else()
    project(TruceAAXTemplate LANGUAGES CXX)
endif()

# AAX SDK path
if(NOT DEFINED AAX_SDK_PATH)
    if(DEFINED ENV{AAX_SDK_PATH})
        set(AAX_SDK_PATH $ENV{AAX_SDK_PATH})
    else()
        message(FATAL_ERROR "Set AAX_SDK_PATH to the AAX SDK root directory.")
    endif()
endif()

# AAX library — use pre-built if available, otherwise build from source.
# `AAX_LIB_PATH` may be passed in via `-D` by truce-xtask (pointing at a
# truce-managed build under `{sdk}/build-truce/`). If not provided, fall
# back to the SDK's conventional `build-lib/` tree.
if(NOT DEFINED AAX_LIB_PATH)
    if(APPLE)
        set(AAX_LIB_PATH "${AAX_SDK_PATH}/build-lib/Libs/AAXLibrary/libAAXLibrary.a")
    else()
        set(AAX_LIB_PATH "${AAX_SDK_PATH}/build-lib/Libs/AAXLibrary/AAXLibrary.lib")
    endif()
endif()

# On macOS: if the pre-built library exists but doesn't carry every
# architecture we were asked to build for (the SDK's default cmake on
# Apple Silicon produces an arm64-only `.a`), ignore it and fall through
# to the source build. Universal plug-ins then get a fat library picked
# up automatically from CMAKE_OSX_ARCHITECTURES.
set(AAX_LIB_USABLE FALSE)
if(EXISTS "${AAX_LIB_PATH}")
    set(AAX_LIB_USABLE TRUE)
    if(APPLE AND CMAKE_OSX_ARCHITECTURES)
        execute_process(
            COMMAND lipo -info "${AAX_LIB_PATH}"
            OUTPUT_VARIABLE _aax_lipo_out
            OUTPUT_STRIP_TRAILING_WHITESPACE
        )
        foreach(arch IN LISTS CMAKE_OSX_ARCHITECTURES)
            if(NOT "${_aax_lipo_out}" MATCHES "${arch}")
                message(STATUS
                    "Pre-built AAX library is missing arch '${arch}'; "
                    "building from source with CMAKE_OSX_ARCHITECTURES="
                    "${CMAKE_OSX_ARCHITECTURES}.")
                set(AAX_LIB_USABLE FALSE)
                break()
            endif()
        endforeach()
    endif()
endif()

if(NOT AAX_LIB_USABLE)
    message(STATUS "Building AAX library from source...")
    # The AAX SDK's Libs/AAXLibrary/CMakeLists.txt expects an
    # AAX_SDK_Interface target (defined in the SDK's root CMakeLists.txt) to
    # propagate the Interfaces/ include dirs. We bypass the root CMakeLists
    # (which would pull in example plugins we don't want). Create a stub
    # AAX_SDK_Interface here so AAXLibrary's PUBLIC link_libraries resolves
    # and the include dirs propagate.
    if(NOT TARGET AAX_SDK_Interface)
        add_library(AAX_SDK_Interface INTERFACE)
        target_include_directories(AAX_SDK_Interface INTERFACE
            ${AAX_SDK_PATH}/Interfaces
            ${AAX_SDK_PATH}/Interfaces/ACF
        )
    endif()
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
elseif(WIN32)
    # Windows AAX plugins are .aaxplugin DLLs that live inside the
    # Contents/x64/ subdirectory of the bundle. cmake's MODULE output
    # defaults to .dll — rename to .aaxplugin.
    set_target_properties(TruceAAXTemplate PROPERTIES
        SUFFIX ".aaxplugin"
        PREFIX ""
    )
endif()

target_compile_features(TruceAAXTemplate PRIVATE cxx_std_17)
