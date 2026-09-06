// Installs the JSI bindings into the app's JavaScript runtime.
//
// The counterpart to `android/src/main/java/.../VdbModule.java`, and it does the same one thing:
// reach the `jsi::Runtime` and call `vdb::install`. Everything a caller uses is on
// `globalThis.__vdb` afterwards, reached from C++ with no bridge in the path.
//
// Its absence is one of the two reasons version 0.1.0 could not build: the podspec listed
// `ios/**/*.{h,mm}` and the directory was empty, so nothing ever installed the bindings and
// `globalThis.__vdb` stayed undefined however the app was configured.
//
// NOT VERIFIED. There is no Xcode here to run `pod install` against. Expect to correct something
// on first use, and please report what.

#import "Vdb.h"

#import <React/RCTBridge+Private.h>
#import <React/RCTUtils.h>
#import <ReactCommon/RCTTurboModule.h>

#import <jsi/jsi.h>

namespace vdb {
/// Defined in `cpp/vdb_jsi.cpp`.
void install(facebook::jsi::Runtime &runtime);
}  // namespace vdb

@implementation Vdb

RCT_EXPORT_MODULE()

/// The bridge, which `setBridge:` records so `install` can reach the runtime through it.
@synthesize bridge = _bridge;

/// Install on the JavaScript thread rather than lazily.
///
/// `requiresMainQueueSetup` is NO because nothing here touches UIKit, and returning YES would
/// make every app pay a main-thread hop at startup for a database.
+ (BOOL)requiresMainQueueSetup {
  return NO;
}

/**
 * Hand the JSI runtime to C++.
 *
 * Synchronous by necessity: the JavaScript that calls it goes on to use `globalThis.__vdb`
 * immediately, and an asynchronous install would leave a window in which it does not exist yet.
 */
RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(install) {
  RCTBridge *bridge = [RCTBridge currentBridge] ?: _bridge;
  if (bridge == nil) {
    return @NO;
  }

  RCTCxxBridge *cxxBridge = (RCTCxxBridge *)bridge;
  if (cxxBridge.runtime == nullptr) {
    // The runtime is not up yet, or this is a bridgeless configuration that does not expose it
    // here. Reported rather than raised so the JavaScript layer's message — which knows about
    // development builds and Expo Go — is the one the developer sees.
    return @NO;
  }

  vdb::install(*(facebook::jsi::Runtime *)cxxBridge.runtime);
  return @YES;
}

@end
