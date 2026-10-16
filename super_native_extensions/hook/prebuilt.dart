// Prebuilt native libraries for build hooks. Identical copies live in
// super_native_extensions and irondash_engine_context; keep them in sync.
//
// By default the hook downloads the library for the build target from the
// package's GitHub release and verifies it against the checksums in
// `prebuilt.g.dart`. Apps opt into compiling the Rust crate instead with:
//
//   hooks:
//     user_defines:
//       <package name>:
//         build_from_source: true

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:code_assets/code_assets.dart';
import 'package:crypto/crypto.dart';
import 'package:hooks/hooks.dart';
import 'package:native_toolchain_rust/native_toolchain_rust.dart';

class PrebuiltLibrary {
  const PrebuiltLibrary({
    required this.package,
    required this.assetName,
    required this.cratePath,
    required this.libName,
    required this.repo,
    required this.triples,
  });

  /// Dart package name; also prefixes the release tag.
  final String package;

  /// Code asset name the Dart side loads.
  final String assetName;

  /// Rust crate directory, relative to the package root.
  final String cratePath;

  /// Library name without platform prefix or extension.
  final String libName;

  /// GitHub `owner/name` that hosts the releases.
  final String repo;

  /// Rust target triples published in each release.
  final List<String> triples;

  String releaseTag(String crateHash) => '$package-native-$crateHash';

  /// Library file name on [triple], matching what `cargo build` produces.
  String libFileName(String triple) {
    if (triple.contains('windows')) return '$libName.dll';
    if (triple.contains('apple')) return 'lib$libName.dylib';
    return 'lib$libName.so';
  }

  /// Release asset name, e.g. `aarch64-apple-ios-libfoo.dylib`.
  String assetFileName(String triple) => '$triple-${libFileName(triple)}';

  Uri downloadUri(String crateHash, String triple) => Uri.https(
        'github.com',
        '/$repo/releases/download/${releaseTag(crateHash)}/'
            '${assetFileName(triple)}',
      );

  Future<void> run(
    BuildInput input,
    BuildOutputBuilder output, {
    required String crateHash,
    required Map<String, String> sha256s,
  }) async {
    if (!input.config.buildCodeAssets) return;

    if (input.userDefines['build_from_source'] == true) {
      await RustBuilder(assetName: assetName, cratePath: cratePath)
          .run(input: input, output: output);
      return;
    }

    final crate = Directory.fromUri(input.packageRoot.resolve(cratePath));
    final (hash, files) = computeCrateHash(crate);
    output.dependencies.addAll(files.map((f) => f.absolute.uri));
    if (hash != crateHash) {
      throw StateError(
        'The Rust sources in ${crate.path} do not match the prebuilt '
        '$package binaries (prebuilt: "$crateHash", sources: "$hash"). '
        'Revert the local Rust changes, or build from source by adding this '
        'to the app pubspec.yaml:\n$_buildFromSourceYaml',
      );
    }

    final triple = targetTriple(input.config.code);
    final expected = sha256s[triple];
    if (triple == null || expected == null) {
      throw UnsupportedError(
        'No prebuilt $package binary for '
        '${input.config.code.targetOS} ${input.config.code.targetArchitecture}'
        ' ($triple). Build from source by adding this to the app '
        'pubspec.yaml:\n$_buildFromSourceYaml',
      );
    }

    // Keyed by checksum so concurrent builds and stale caches never collide.
    final file = File.fromUri(input.outputDirectoryShared
        .resolve('${expected.substring(0, 16)}/${libFileName(triple)}'));
    if (!file.existsSync() || _sha256(file.readAsBytesSync()) != expected) {
      final uri = downloadUri(crateHash, triple);
      final bytes = await download(uri);
      final actual = _sha256(bytes);
      if (actual != expected) {
        throw StateError(
          'Checksum mismatch for $uri: expected $expected, got $actual.',
        );
      }
      file.parent.createSync(recursive: true);
      File('${file.path}.$pid.tmp')
        ..writeAsBytesSync(bytes)
        ..renameSync(file.path);
    }

    output.assets.code.add(CodeAsset(
      package: input.packageName,
      name: assetName,
      linkMode: DynamicLoadingBundled(),
      file: file.uri,
    ));
  }

  String get _buildFromSourceYaml => '''
hooks:
  user_defines:
    $package:
      build_from_source: true''';
}

/// Rust target triple for the hook's build target, or null if unsupported.
String? targetTriple(CodeConfig code) =>
    switch ((code.targetOS, code.targetArchitecture)) {
      (OS.android, Architecture.arm64) => 'aarch64-linux-android',
      (OS.android, Architecture.arm) => 'armv7-linux-androideabi',
      (OS.android, Architecture.x64) => 'x86_64-linux-android',
      (OS.iOS, Architecture.arm64)
          when code.iOS.targetSdk == IOSSdk.iPhoneSimulator =>
        'aarch64-apple-ios-sim',
      (OS.iOS, Architecture.arm64) => 'aarch64-apple-ios',
      (OS.iOS, Architecture.x64) => 'x86_64-apple-ios',
      (OS.macOS, Architecture.arm64) => 'aarch64-apple-darwin',
      (OS.macOS, Architecture.x64) => 'x86_64-apple-darwin',
      (OS.windows, Architecture.arm64) => 'aarch64-pc-windows-msvc',
      (OS.windows, Architecture.x64) => 'x86_64-pc-windows-msvc',
      (OS.linux, Architecture.arm64) => 'aarch64-unknown-linux-gnu',
      (OS.linux, Architecture.x64) => 'x86_64-unknown-linux-gnu',
      _ => null,
    };

/// Hash identifying the crate sources, and the files it covers.
///
/// Skips `target/` and dotfiles and normalizes CRLF, so a git checkout gives
/// the same hash on every OS regardless of `core.autocrlf`.
(String, List<File>) computeCrateHash(Directory crate) {
  String relative(File f) =>
      f.path.substring(crate.path.length + 1).replaceAll(r'\', '/');

  final files = crate
      .listSync(recursive: true, followLinks: false)
      .whereType<File>()
      .where((f) => !relative(f)
          .split('/')
          .any((part) => part == 'target' || part.startsWith('.')))
      .toList()
    ..sort((a, b) => relative(a).compareTo(relative(b)));

  final manifest = StringBuffer();
  for (final file in files) {
    manifest.writeln(
        '${relative(file)} ${_sha256(_withoutCr(file.readAsBytesSync()))}');
  }
  final hash = _sha256(utf8.encode(manifest.toString())).substring(0, 16);
  return (hash, files);
}

Future<Uint8List> download(Uri uri) async {
  final client = HttpClient()
    ..findProxy = HttpClient.findProxyFromEnvironment;
  try {
    final response = await (await client.getUrl(uri)).close();
    if (response.statusCode != HttpStatus.ok) {
      throw HttpException('HTTP ${response.statusCode}', uri: uri);
    }
    final bytes = BytesBuilder(copy: false);
    await response.forEach(bytes.add);
    return bytes.takeBytes();
  } finally {
    client.close();
  }
}

String _sha256(List<int> bytes) => sha256.convert(bytes).toString();

List<int> _withoutCr(Uint8List bytes) => [
      for (var i = 0; i < bytes.length; i++)
        if (!(bytes[i] == 13 && i + 1 < bytes.length && bytes[i + 1] == 10))
          bytes[i],
    ];
