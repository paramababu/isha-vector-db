# Android packaging.
#
# NOT VERIFIED. This has never been through a Gradle build — there is no Android project here to
# try it against. Treat it as a starting point.
#
# libisha_vector_db_ffi.a for each ABI comes from `scripts/build-android.sh`.
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
  ../include
  ${REACT_NATIVE_DIR}/ReactCommon/jsi
)

# The Rust static library, one per ABI.
add_library(isha_vector_db_ffi STATIC IMPORTED)
set_target_properties(isha_vector_db_ffi PROPERTIES
  IMPORTED_LOCATION ${CMAKE_CURRENT_SOURCE_DIR}/jniLibs/${ANDROID_ABI}/libisha_vector_db_ffi.a)

target_link_libraries(vdb isha_vector_db_ffi android log)
