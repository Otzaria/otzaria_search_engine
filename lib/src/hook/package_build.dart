import 'cargo_build.dart';
import 'native_build.dart';
import 'precompiled.dart';

/// Flutter 3.38.1 writes MinimumOSVersion=12.0 into native iOS frameworks.
/// A device binary requiring a newer OS makes App Store validation reject it.
/// Keep this in the hashed configuration so changing it publishes a new set.
const precompiledIosVersion = '12.0';

/// Rust's ARM64 simulator target requires iOS 14. Simulator binaries never
/// enter a device archive, so they do not share the device framework's ceiling.
const precompiledIosArm64SimulatorVersion = '14.0';

/// The engine library, shared by `hook/build.dart` and `tool/precompiled_binaries`.
///
/// The app shares Dart's SQLite (`sqlite-host`), so the bundled default is off;
/// the two features must never combine.
const nativeBuild = NativeBuild(
  assetName: 'src/rust/frb_generated.io.dart',
  cargo: CargoOptions(
    libName: 'search_engine',
    noDefaultFeatures: true,
    features: ['semantic', 'sqlite-host'],
  ),
  precompiled: PrecompiledBinaries(
    urlPrefix:
        'https://github.com/otzaria/otzaria_search_engine/releases/download/precompiled_',
    publicKeyHex:
        '8b1d5cbe1a2eb4ba0face7d49f68cb75d29bbf1f7d1a569c42d277d20ea65316',
  ),
  extraCrateDirs: ['resources', 'vendor'],
);
