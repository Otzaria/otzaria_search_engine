export 'src/rust/api/search_engine.dart';
export 'src/rust/frb_generated.dart' show RustLib;

/// חשוף עבור צרכנים שצריכים לטעון את הספרייה הנייטיבית בעצמם (למשל אתחול
/// RustLib ב-isolate משני או בטסטים) בלי לתלות ישירות ב-flutter_rust_bridge.
export 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
