# CocoaPods packaging for iOS.
#
# NOT VERIFIED BY CI. This has never been through `pod install` against a real project — there is
# no Xcode here to try it with. What *is* checked, by `scripts/check-react-native-package.sh`, is
# that every path named below exists in the published tarball; in 0.1.0 two of them did not,
# which is why the package could not build at all.
#
# Treat the rest as a starting point and expect to correct something.
#
# Vdb.xcframework is produced by `scripts/build-xcframework.sh` and placed in ios/ by
# `scripts/build-react-native.sh`.
require 'json'

package = JSON.parse(File.read(File.join(__dir__, 'package.json')))

Pod::Spec.new do |s|
  s.name         = 'isha-vector-db'
  s.version      = package['version']
  s.summary      = package['description']
  s.license      = package['license']
  s.authors      = 'isha-vector-db'
  s.homepage     = 'https://github.com/paramababu/isha-vector-db'
  s.platforms    = { ios: '13.4' }
  s.source       = { git: 'https://github.com/paramababu/isha-vector-db.git', tag: s.version.to_s }

  # cpp/ holds the bridge and the JSI layer, shared verbatim with Android. ios/ holds only the
  # module that installs them.
  s.source_files = 'cpp/**/*.{h,cpp}', 'ios/**/*.{h,mm}'

  # The engine itself: a static library inside an XCFramework, one slice per architecture.
  s.vendored_frameworks = 'ios/Vdb.xcframework'

  s.pod_target_xcconfig = {
    # cpp/ for vdb_bridge.h, include/ for the C ABI header it includes. include/vdb.h is copied
    # in from crates/isha-vector-db-ffi/include at pack time so the tarball is self-contained.
    'HEADER_SEARCH_PATHS' => '"$(PODS_TARGET_SRCROOT)/cpp" "$(PODS_TARGET_SRCROOT)/include"',
    'CLANG_CXX_LANGUAGE_STANDARD' => 'c++17',
    # The bridge translates every engine failure into a returned value, but jsi::JSError is
    # thrown, so exceptions cannot be disabled here.
    'GCC_ENABLE_CPP_EXCEPTIONS' => 'YES',
  }

  # Pulls in React-Core, React-jsi and the rest at whatever version the app is on. Without it a
  # New Architecture app links against nothing and every JSI symbol is unresolved.
  if respond_to?(:install_modules_dependencies, true)
    install_modules_dependencies(s)
  else
    # React Native before 0.71 had no helper; name the two pods this actually needs.
    s.dependency 'React-Core'
    s.dependency 'React-jsi'
  end
end
