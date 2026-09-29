// Regenerates the FlatBuffers bindings the shim reads, from the schemas beside
// it.
//
//   dart tool/generate_bindings.dart
//
// Run this after editing any `.fbs` file, and commit the output: the generated
// Rust is in the repository so that a checkout builds without flatc installed.
//
// The schemas live here because the shim does. They describe the wire format it
// reads — not the format the caller writes, which is the same thing seen from
// the other side — and the shim is compiled from this repository for both the
// native library and the WebAssembly module. The Dart half of the bindings is
// generated on the other side, from these files, by the consumer's
// `tool/generate_bindings.dart`; it takes them from the checkout its build
// pins, so the two sides cannot be generated from different revisions of a
// schema.
//
// A plain Dart script rather than a package: it imports `dart:io` and nothing
// else, so `dart tool/generate_bindings.dart` runs it with no pubspec, no
// dependencies and no `.dart_tool` in a Rust repository.
//
// # The output depends on the flatc version, not just its major
//
// The check below is `25 or newer`, and that is not enough to reproduce what is
// committed: 25.2.10 emits the `use` list in a different order from whichever
// 25.x produced the files that are here, which is a diff of several thousand
// lines that means nothing. It compiles either way — this is churn and not a
// break — but regenerating rewrites every file, so run this when a schema has
// changed and expect the rest to come along.
import 'dart:io';

/// The schemas, and the crate whose generated code they feed.
const String _schemaDir = 'crates/ffi/schema';
const String _rustOutDir = 'crates/ffi/src/generated';

/// The emitter changed incompatibly between 24.x and 25.x; 24.x output does not
/// compile against the `flatbuffers` crate version this workspace pins.
const int _minimumFlatcMajor = 25;

void main(List<String> args) {
  final Directory root = Directory.current;
  final Directory? schemaDir = _schemaDirOf(root);
  if (schemaDir == null) {
    return;
  }

  final String flatc = _resolveFlatc();
  final List<File> schemas = schemaDir
      .listSync()
      .whereType<File>()
      .where((File f) => f.path.endsWith('.fbs'))
      .toList()
    ..sort((File a, File b) => a.path.compareTo(b.path));
  if (schemas.isEmpty) {
    stderr.writeln('No .fbs schemas found under ${schemaDir.path}.');
    exitCode = 1;
    return;
  }

  final Directory staging = Directory.systemTemp.createTempSync('quieditor_fbs');
  try {
    final ProcessResult result = Process.runSync(flatc, <String>[
      '--rust',
      '-o',
      staging.path,
      ...schemas.map((File f) => f.path),
    ]);
    if (result.exitCode != 0) {
      stderr
        ..writeln('flatc failed (${result.exitCode}):')
        ..writeln(result.stdout)
        ..writeln(result.stderr);
      exitCode = result.exitCode;
      return;
    }

    final Directory rustOut = Directory('${root.path}/$_rustOutDir')
      ..createSync(recursive: true);

    final List<File> staged = staging.listSync().whereType<File>().toList();
    for (final File schema in schemas) {
      final String base = _basenameWithoutExtension(schema.path);
      final File? rust = _findStaged(staged, base, '_generated.rs');
      if (rust == null) {
        stderr.writeln(
          'flatc produced no Rust output for ${base}.fbs. The emitter names '
          'its file after the namespace; check the schema declares one.',
        );
        exitCode = 1;
        return;
      }
      File('${rustOut.path}/${base}_generated.rs')
          .writeAsStringSync(rust.readAsStringSync());
    }

    _writeModuleIndex(rustOut, schemas);
    stdout.writeln(
      'Regenerated ${schemas.length} binding file(s) from '
      '${schemas.length} schema(s) in ${schemaDir.path}.',
    );
  } finally {
    staging.deleteSync(recursive: true);
  }
}

/// The schema directory, or null after saying why it is not there.
Directory? _schemaDirOf(Directory root) {
  final Directory dir = Directory('${root.path}/$_schemaDir');
  if (!dir.existsSync()) {
    stderr.writeln(
      'No $_schemaDir here. Run this from the root of the quieditor_engine '
      'checkout.',
    );
    exitCode = 1;
    return null;
  }
  return dir;
}

String _resolveFlatc() {
  final String? override = Platform.environment['FLATC'];
  final String candidate;
  if (override != null && override.isNotEmpty) {
    candidate = override;
  } else {
    final ProcessResult which = Process.runSync('which', <String>['flatc']);
    if (which.exitCode != 0) {
      stderr.writeln(
        'flatc not found. Install FlatBuffers (brew install flatbuffers) or set '
        'the FLATC environment variable.',
      );
      exit(1);
    }
    candidate = (which.stdout as String).trim();
  }

  final ProcessResult version = Process.runSync(candidate, <String>['--version']);
  final String reported = '${version.stdout}'.trim();
  final int? major = int.tryParse(
    RegExp(r'(\d+)\.').firstMatch(reported)?.group(1) ?? '',
  );
  if (major == null || major < _minimumFlatcMajor) {
    stderr.writeln(
      'flatc at $candidate reports "$reported", which is older than the '
      '$_minimumFlatcMajor.x this workspace needs.\n'
      'Its output will not compile against the pinned flatbuffers crate. '
      'Install a newer one and set FLATC.',
    );
    exit(1);
  }
  return candidate;
}

/// Finds the staged output whose name starts with `<base>_` and ends with
/// [suffix]. The prefix has to be anchored, or `abi` would also match a longer
/// schema name like `abi_v2`.
File? _findStaged(List<File> staged, String base, String suffix) {
  for (final File file in staged) {
    final String name = file.uri.pathSegments.last;
    if (name.startsWith('${base}_') && name.endsWith(suffix)) {
      return file;
    }
  }
  return null;
}

String _basenameWithoutExtension(String path) {
  final String name = path.split(Platform.pathSeparator).last;
  final int dot = name.lastIndexOf('.');
  return dot <= 0 ? name : name.substring(0, dot);
}

/// The Rust side needs a module per schema, listed in one place.
void _writeModuleIndex(Directory rustOut, List<File> schemas) {
  final StringBuffer buffer = StringBuffer()
    ..writeln('//! FlatBuffers tables generated by `flatc` from '
        '`crates/ffi/schema`.')
    ..writeln('//!')
    ..writeln('//! Regenerate with `dart tool/generate_bindings.dart`; do not '
        'edit.')
    ..writeln('//!')
    ..writeln('//! The generated code is not ours to keep tidy, and it predates')
    ..writeln('//! several current idioms, so the lints it trips are switched '
        'off here')
    ..writeln('//! rather than in the crate root — this keeps them on for the '
        'hand-written')
    ..writeln('//! shim, where they are worth having.')
    ..writeln('//!')
    ..writeln('//! Notably `unsafe_op_in_unsafe_fn`: flatc emits bodies that '
        'rely on the')
    ..writeln('//! pre-2024 rule that the body of an `unsafe fn` is implicitly '
        'unsafe.')
    ..writeln('#![allow(')
    ..writeln('    clippy::all,')
    ..writeln('    dead_code,')
    ..writeln('    mismatched_lifetime_syntaxes,')
    ..writeln('    missing_debug_implementations,')
    ..writeln('    unsafe_op_in_unsafe_fn,')
    ..writeln('    unused_imports')
    ..writeln(')]')
    ..writeln();
  for (final File schema in schemas) {
    buffer.writeln('pub mod ${_basenameWithoutExtension(schema.path)}_generated;');
  }
  File('${rustOut.path}/mod.rs').writeAsStringSync(buffer.toString());
}
