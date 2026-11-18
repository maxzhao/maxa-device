#!/usr/bin/env python3
"""Archive only public source inputs for the combined APK and embedded Agent."""
import gzip
import os
from pathlib import Path
import sys
import tarfile

ROOT = Path(__file__).resolve().parents[1]
FILES = {
    'LICENSE', 'THIRD-PARTY.md', 'src/locales/en.json',
    'android/build-companion.sh', 'android/archive-companion-source.py',
    'android/agent/build.sh', 'android/agent/install.sh.in',
    'android/companion/build.gradle.kts', 'android/companion/settings.gradle.kts',
    'android/companion/EMBEDDED-API.md',
    'android/companion/gradle/verification-metadata.xml',
    'android/companion/app/build.gradle.kts', 'android/companion/app/gradle.lockfile',
    'src-tauri/Cargo.toml', 'src-tauri/Cargo.lock', 'src-tauri/build.rs',
}
TREES = {
    'android/companion/app/src': {'.java', '.kt', '.xml', '.png', '.webp'},
    'android/companion/licenses': {'.txt', '.json', '.md'},
    'src-tauri/src': {'.rs', '.json'},
    'src-tauri/vendor/adbutils-rs': {'.rs', '.toml', '.json', '.md'},
    'licenses/rust-supplements': None,
}


def public_files():
    files = {ROOT / name for name in FILES}
    files.add(ROOT / 'src-tauri/vendor/adbutils-rs/LICENSE')
    files.update((ROOT / 'licenses').glob('*-LICENSE'))
    files.update((ROOT / 'licenses').glob('*-NOTICE'))
    files.update((ROOT / 'licenses').glob('*.json'))
    for name, suffixes in TREES.items():
        for path in (ROOT / name).rglob('*'):
            rel = path.relative_to(ROOT / name)
            if any(part.startswith('.') for part in rel.parts):
                continue
            if path.is_file() and (suffixes is None or path.suffix in suffixes):
                files.add(path)
    for path in sorted(files):
        if path.resolve() != path or not path.is_file():
            raise SystemExit(f'Non-regular source input: {path.relative_to(ROOT)}')
        yield path


def main():
    if len(sys.argv) != 2:
        raise SystemExit('Usage: archive-companion-source.py <new source tar.gz>')
    out = Path(sys.argv[1])
    with out.open('xb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as packed:
        with tarfile.open(mode='w', fileobj=packed, format=tarfile.PAX_FORMAT) as archive:
            for path in public_files():
                entry = tarfile.TarInfo(str(Path('maxa-device-companion-source') / path.relative_to(ROOT)))
                entry.size = path.stat().st_size
                entry.mode = 0o755 if os.access(path, os.X_OK) else 0o644
                with path.open('rb') as src:
                    archive.addfile(entry, src)


if __name__ == '__main__':
    main()
