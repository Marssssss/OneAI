# HarmonyOS NAPI module for OneAI. Links liboneai.so (built by
# scripts/build_harmony.sh, staged at entry/src/main/cpp/libs/<abi>/) +
# the oneai_c.h facade header (../../../../../bindings/c/).
#
# Built by DevEco's hvigor (invokes CMake with the OHOS NDK toolchain).

cmake_minimum_required(VERSION 3.16)
project(oneai_napi)

set(CMAKE_CXX_STANDARD 17)

include_directories(
    ${CMAKE_CURRENT_SOURCE_DIR}
    # oneai_c.h lives at the repo root bindings/c/
    ${CMAKE_CURRENT_SOURCE_DIR}/../../../../../bindings/c
)

add_library(oneai_napi SHARED
    napi_init.cpp
)

# liboneai.a (staticlib) is staged per-ABI by scripts/build_harmony.sh and
# linked statically into liboneai_napi.so (one self-contained NAPI .so).
set(ONEAI_LIB_DIR ${CMAKE_CURRENT_SOURCE_DIR}/libs/${CMAKE_OHOS_ARCH_ABI})
if(EXISTS "${ONEAI_LIB_DIR}/liboneai.a")
    target_link_libraries(oneai_napi PRIVATE ${ONEAI_LIB_DIR}/liboneai.a)
else()
    message(WARNING "liboneai.a not found at ${ONEAI_LIB_DIR} — run scripts/build_harmony.sh first.")
endif()

# OHOS NAPI runtime
target_link_libraries(oneai_napi PRIVATE hilog_ndk.z)
