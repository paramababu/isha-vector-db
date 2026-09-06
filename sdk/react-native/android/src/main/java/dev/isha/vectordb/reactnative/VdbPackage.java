package dev.isha.vectordb.reactnative;

import com.facebook.react.ReactPackage;
import com.facebook.react.bridge.NativeModule;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.uimanager.ViewManager;

import java.util.Collections;
import java.util.List;

/**
 * The entry point autolinking registers.
 *
 * <p>React Native finds this by reading {@code android/build.gradle} and the package's
 * {@code react-native.config.js}; without both, nothing here is ever constructed.
 *
 * <p><b>Not verified.</b> See the README.
 */
public class VdbPackage implements ReactPackage {
  @Override
  public List<NativeModule> createNativeModules(ReactApplicationContext context) {
    return Collections.<NativeModule>singletonList(new VdbModule(context));
  }

  @Override
  public List<ViewManager> createViewManagers(ReactApplicationContext context) {
    // No views. This is a database.
    return Collections.emptyList();
  }
}
