# HarmonyOS NAPI module for OneAI. Links liboneai.so (built by
# scripts/build_harmony.sh, staged at entry/src/main/cpp/libs/<abi>/) +
# the oneai_c.h facade header (../../../../../bindings/c/).
#
# Built by DevEco's hvigor (invokes CMake with the OHOS NDK toolchain).

cmake_minimum_required(VERSION 3.16)
project(oneai_napi)

set(CMAKE_CXX_STANDARD 17)

# Locate the repo root by walking up until we find bindings/c/oneai_c.h
# (robust to the NAPI CMakeLists being moved within the tree).
set(_search_dir ${CMAKE_CURRENT_SOURCE_DIR})
while(NOT EXISTS "${_search_dir}/bindings/c/oneai_c.h")
    get_filename_component(_parent ${_search_dir} DIRECTORY)
    if(_parent STREQUAL _search_dir OR _parent STREQUAL "")
        message(FATAL_ERROR
            "bindings/c/oneai_c.h not found walking up from ${CMAKE_CURRENT_SOURCE_DIR}. "
            "Is the HarmonyOS project inside the OneAI repo?")
    endif()
    set(_search_dir ${_parent})
endwhile()
set(ONEAI_ROOT ${_search_dir})
message(STATUS "OneAI repo root: ${ONEAI_ROOT}")

include_directories(
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${ONEAI_ROOT}/bindings/c
)

add_library(oneai_napi SHARED
    napi_init.cpp
)

# liboneai.a (staticlib) is staged per-ABI by scripts/build_harmony.sh and
# linked statically into liboneai_napi.so (one self-contained NAPI .so).
# CMAKE_OHOS_ARCH_ABI is set by the OHOS NDK toolchain (arm64-v8a / x86_64).
if(DEFINED CMAKE_OHOS_ARCH_ABI)
    set(ONEAI_LIB_DIR ${CMAKE_CURRENT_SOURCE_DIR}/libs/${CMAKE_OHOS_ARCH_ABI})
elseif(DEFINED OHOS_ARCH)
    set(ONEAI_LIB_DIR ${CMAKE_CURRENT_SOURCE_DIR}/libs/${OHOS_ARCH})
else()
    set(ONEAI_LIB_DIR ${CMAKE_CURRENT_SOURCE_DIR}/libs/${CMAKE_LIBRARY_ARCHITECTURE})
endif()
if(EXISTS "${ONEAI_LIB_DIR}/liboneai.a")
    target_link_libraries(oneai_napi PRIVATE ${ONEAI_LIB_DIR}/liboneai.a)
else()
    message(FATAL_ERROR
        "liboneai.a not found at ${ONEAI_LIB_DIR}.\n"
        "Build the Rust staticlib first:\n"
        "  export OHOS_NDK_HOME=/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony/native\n"
        "  rustup target add aarch64-linux-ohos x86_64-linux-ohos\n"
        "  ./scripts/build_harmony.sh")
endif()

# OHOS NAPI runtime (libace_napi.z.so — provides napi_* symbols) + hilog.
target_link_libraries(oneai_napi PRIVATE ace_napi.z hilog_ndk.z)
