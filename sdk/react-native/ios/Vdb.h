// The iOS native module that installs the JSI bindings.
//
// NOT VERIFIED. There is no Xcode project here to build it against. See the README's table of
// what is tested and what is not.

#import <React/RCTBridgeModule.h>

@interface Vdb : NSObject <RCTBridgeModule>
@end
