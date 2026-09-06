package dev.isha.vectordb.reactnative;

import com.facebook.react.bridge.JavaScriptContextHolder;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReactContextBaseJavaModule;
import com.facebook.react.bridge.ReactMethod;
import com.facebook.react.module.annotations.ReactModule;

/**
 * Installs the JSI bindings into the app's JavaScript runtime.
 *
 * <p>This module exists only to run {@code vdb::install} once at startup. Everything a caller
 * actually uses is on {@code globalThis.__vdb} afterwards, reached directly from C++ with no
 * bridge in the path — which is the entire reason for using JSI here, since the legacy bridge
 * would JSON-serialise every vector.
 *
 * <p><b>Not verified.</b> There is no Android project in this repository to build it against.
 * See the README's table of what is tested and what is not.
 */
@ReactModule(name = VdbModule.NAME)
public class VdbModule extends ReactContextBaseJavaModule {
  public static final String NAME = "Vdb";

  static {
    // "vdb" is the library CMakeLists.txt builds. A failure here is an ABI that was not built,
    // and it is worth letting it throw: the alternative is a module that loads and then reports
    // "not installed" from JavaScript, which sends people looking in the wrong place.
    System.loadLibrary("vdb");
  }

  public VdbModule(ReactApplicationContext context) {
    super(context);
  }

  @Override
  public String getName() {
    return NAME;
  }

  /**
   * Hand the JSI runtime to C++.
   *
   * <p>Synchronous by necessity: the JavaScript that calls it goes on to use
   * {@code globalThis.__vdb} immediately, and an asynchronous install would leave a window in
   * which the property does not exist yet.
   *
   * @return whether the bindings were installed.
   */
  @ReactMethod(isBlockingSynchronousMethod = true)
  public boolean install() {
    try {
      JavaScriptContextHolder runtime = getReactApplicationContext().getJavaScriptContextHolder();
      if (runtime == null || runtime.get() == 0) {
        // Bridgeless mode holds no context here. Reported rather than thrown so the JavaScript
        // layer's message — which knows about development builds and Expo Go — is the one the
        // developer sees.
        return false;
      }
      nativeInstall(runtime.get());
      return true;
    } catch (UnsatisfiedLinkError e) {
      return false;
    }
  }

  private static native void nativeInstall(long runtimePointer);
}
