# CocoaPods packaging for iOS.
#
# NOT VERIFIED. This has never been through `pod install` against a real project — there is no
# Xcode here to try it with. Treat it as a starting point and expect to correct something.
#
# The static library it references is produced by `scripts/build-xcframework.sh`.
require 'json'

package = JSON.parse(File.read(File.join(__dir__, 'package.json')))

Pod::Spec.new do |s|
  s.name         = 'isha-vector-db'
  s.version      = package['version']
  s.summary      = package['description']
  s.license      = package['license']
  s.authors      = 'vdb'
  s.homepage     = 'https://github.com/paramababu/isha-vector-db'
  s.platforms    = { ios: '13.4' }
  s.source       = { git: 'https://github.com/paramababu/isha-vector-db.git' }

  s.source_files = 'cpp/**/*.{h,cpp}', 'ios/**/*.{h,mm}'
  s.vendored_frameworks = 'ios/Vdb.xcframework'

  # The C ABI header, shared with every other binding.
  s.pod_target_xcconfig = {
    'HEADER_SEARCH_PATHS' => '"$(PODS_TARGET_SRCROOT)/cpp" "$(PODS_TARGET_SRCROOT)/include"',
    'CLANG_CXX_LANGUAGE_STANDARD' => 'c++17',
  }

  install_modules_dependencies(s) if respond_to?(:install_modules_dependencies)
end
