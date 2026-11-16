# Third-party distribution

Maxa Device's own code is licensed under Apache-2.0. Dependencies retain their original licenses; the project license does not relicense vendored code or bundled executables.

## Installed components

| Component | Source/version | Distribution materials |
| --- | --- | --- |
| Rust backend and Tauri | Exact versions and registry checksums in `src-tauri/Cargo.lock` | `licenses/THIRD-PARTY-NOTICES.txt`; matching package sources in the release's `third-party-sources.tar.gz` |
| Tauri JavaScript API and Vite runtime helpers | Exact versions in `package-lock.json` | Same notices and source archive |
| ripgrep | 15.1.0, official Windows MSVC archive | Original `COPYING`, `LICENSE-MIT`, and `UNLICENSE` in the System pack; upstream URL and archive SHA-256 in `components.json` |
| Android Debug Bridge | Platform Tools 37.0.1, official Windows archive | Only unchanged `adb.exe`, `AdbWinApi.dll`, `AdbWinUsbApi.dll`, `NOTICE.txt`, and `source.properties`; upstream URL and archive SHA-256 in `components.json` |
| Android helper | uiautomator-server-jar 0.4.0, local build `0.4.0-maxa.3`, MIT | `licenses/u2-server-LICENSE`, `licenses/u2-server-NOTICE`, upstream commit and hashes, patches and reproducible build inputs in `vendor/u2-server/` |
| Vendored adbutils-rs | 0.1.0 with documented local changes, MIT | `src-tauri/vendor/adbutils-rs/LICENSE` and `MAXA_UPSTREAM.json`; source also included in the dependency-source archive |
| Ported automation implementations | Windows-MCP (MIT), Mobile-MCP (Apache-2.0) | Original attribution/license files in `licenses/` |

The Mobile pack deliberately excludes `fastboot`, `mke2fs`, F2FS tools, SQLite tools, and `libwinpthread`. They are not required by the App and are not redistributed. Google's complete aggregate `NOTICE.txt` is retained unmodified; notices for omitted tools are not a claim that those tools ship in this product. The included ADB binaries import only the two bundled ADB libraries and Windows system libraries.

## Release obligations

Before building an installer, run `npm run prepare:licenses`. It reads the locked Windows dependency graph, retains dependency license texts, and creates the matching dependency-source archive, including the MPL-2.0 packages. Generated notices are bundled with the installer. Publish `third-party-sources.tar.gz` with that same release; do not substitute sources from another lockfile or version.

Each public release also includes `LICENSE`, this file, `components.json`, the built pack manifest, and SHA-256 checksums. Upstream binary archives and all dependency locks remain pinned. Keep required attribution files and source access when redistributing the release. No proprietary Windows SDK, compiler, WebView2 installer, or developer credentials are copied into the component packs.
