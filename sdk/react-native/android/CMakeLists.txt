# Android packaging.
#
# NOT VERIFIED BY CI. There is no Android project here to run Gradle against and no NDK on the
# machine that wrote it. What *is* checked, by `scripts/check-react-native-package.sh`, is that
# every file named below is actually in the published tarball — because two of them were not in
# 0.1.0, and that is why it could not build.
#
# libisha_vector_db_ffi.a for each ABI comes from `scripts/build-android.sh` and is placed in
# jniLibs/<abi>/ by `scripts/build-react-native.sh`.
cmake_minimum_required(VERSION 3.13)
project(vdb)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_library(vdb SHARED
  ../cpp/vdb_bridge.cpp
  ../cpp/vdb_jsi.cpp
  src/main/cpp/vdb_module.cpp
)

target_include_directories(vdb PRIVATE
  ../cpp
  # The C ABI header, copied in from crates/isha-vector-db-ffi/include at pack time so the
  # tarball is self-contained.
  ../include
  ${REACT_NATIVE_DIR}/ReactCommon/jsi
)

# The Rust static library, one per ABI.
add_library(isha_vector_db_ffi STATIC IMPORTED)
set_target_properties(isha_vector_db_ffi PROPERTIES
  IMPORTED_LOCATION ${CMAKE_CURRENT_SOURCE_DIR}/jniLibs/${ANDROID_ABI}/libisha_vector_db_ffi.a)

# A missing archive otherwise surfaces as an unresolved symbol from somewhere inside the engine,
# which sends people looking in the wrong place entirely.
if(NOT EXISTS ${CMAKE_CURRENT_SOURCE_DIR}/jniLibs/${ANDROID_ABI}/libisha_vector_db_ffi.a)
  message(FATAL_ERROR
    "No libisha_vector_db_ffi.a for ${ANDROID_ABI}. The published package ships arm64-v8a, "
    "armeabi-v7a and x86_64; building for another ABI needs scripts/build-android.sh.")
endif()

# React Native provides jsi as a shared library the app already loads. Linking it rather than
# leaving the symbols unresolved is what stops a runtime UnsatisfiedLinkError on the device.
find_library(JSI_LIB jsi PATHS ${REACT_NATIVE_DIR}/ReactAndroid/build/intermediates NO_DEFAULT_PATH)
find_package(ReactAndroid QUIET CONFIG)
if(ReactAndroid_FOUND)
  # React Native 0.76 and later expose one merged target.
  if(TARGET ReactAndroid::reactnative)
    target_link_libraries(vdb ReactAndroid::reactnative ReactAndroid::jsi)
  else()
    target_link_libraries(vdb ReactAndroid::react_nativemodule_core ReactAndroid::jsi)
  endif()
elseif(JSI_LIB)
  target_link_libraries(vdb ${JSI_LIB})
endif()

# Android 15 runs on devices with 16 KB memory pages, and a shared library linked for 4 KB will
# not load there at all. This has to be here rather than on the Rust build: the package ships a
# static archive, and libvdb.so — the thing that is actually loaded — is linked by this file.
# Forgetting it is discovered by a one-star review, not by a test.
target_link_options(vdb PRIVATE "-Wl,-z,max-page-size=16384")

target_link_libraries(vdb isha_vector_db_ffi android log)
