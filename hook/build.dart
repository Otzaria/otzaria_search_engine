import 'package:hooks/hooks.dart';

import 'src/cargo_build.dart';
import 'src/native_build.dart';
import 'src/precompiled.dart';

/// The engine library, shared by the build hook and `tool/precompiled_binaries`.
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

void main(List<String> args) async {
  await build(args, nativeBuild.run);
}
