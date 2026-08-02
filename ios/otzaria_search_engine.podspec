#
# To learn more about a Podspec see http://guides.cocoapods.org/syntax/podspec.html.
# Run `pod lib lint otzaria_search_engine.podspec` to validate before publishing.
#
Pod::Spec.new do |s|
  s.name             = 'otzaria_search_engine'
  # flutter_rust_bridge מחפש את הספרייה לפי שם ה-Rust crate (search_engine).
  # בלי זה ה-framework נבנה בשם ה-pod (otzaria_search_engine) ו-RustLib.init נתקע.
  s.module_name      = 'search_engine'
  s.version          = '0.0.1'
  s.summary          = 'A new Flutter FFI plugin project.'
  s.description      = <<-DESC
A new Flutter FFI plugin project.
                       DESC
  s.homepage         = 'http://example.com'
  s.license          = { :file => '../LICENSE' }
  s.author           = { 'Your Company' => 'email@example.com' }

  # This will ensure the source files in Classes/ are included in the native
  # builds of apps using this FFI plugin. Podspec does not support relative
  # paths, so Classes contains a forwarder C file that relatively imports
  # `../src/*` so that the C sources can be shared among all target platforms.
  s.source           = { :path => '.' }
  s.source_files = 'Classes/**/*'
  s.dependency 'Flutter'
  s.platform = :ios, '11.0'

  # ggml/llama.cpp שבתוך libsearch_engine.a הוא C++ שמשתמש ב-vDSP וב-Metal.
  # cargokit בונה staticlib, כך שהצהרות cargo:rustc-link-lib של llama-cpp-sys-2
  # לא מגיעות ללינקר של Xcode - חובה להצהיר עליהן כאן.
  s.libraries = 'c++'
  s.frameworks = 'Accelerate', 'Metal', 'MetalKit', 'Foundation'

  s.swift_version = '5.0'

  s.script_phase = {
    :name => 'Build Rust library',
    # First argument is relative path to the `rust` folder, second is name of rust library
    :script => 'sh "$PODS_TARGET_SRCROOT/../cargokit/build_pod.sh" ../rust search_engine',
    :execution_position => :before_compile,
    :input_files => ['${BUILT_PRODUCTS_DIR}/cargokit_phony'],
    # Let XCode know that the static library referenced in -force_load below is
    # created by this build step.
    # cargokit כותב ל-$PODS_CONFIGURATION_BUILD_DIR/$PRODUCT_NAME, ו-module_name
    # שינה את PRODUCT_NAME ל-search_engine - לכן הנתיב הזה ולא ${BUILT_PRODUCTS_DIR}.
    :output_files => ["${PODS_CONFIGURATION_BUILD_DIR}/${PRODUCT_NAME}/libsearch_engine.a"],
  }
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    # Flutter.framework does not contain a i386 slice.
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
    'OTHER_LDFLAGS' => '-force_load ${PODS_CONFIGURATION_BUILD_DIR}/${PRODUCT_NAME}/libsearch_engine.a',
  }
end