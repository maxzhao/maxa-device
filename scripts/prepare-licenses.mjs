// Build the notices and corresponding dependency sources shipped with a Windows release.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const release = join(root, 'release');
const notices = join(root, 'licenses', 'THIRD-PARTY-NOTICES.txt');
mkdirSync(release, { recursive: true });
const run = (program, args) => execFileSync(program, args, { cwd: root, maxBuffer: 64 * 1024 * 1024 });
const metadata = JSON.parse(run('cargo', [
  'metadata', '--manifest-path', 'src-tauri/Cargo.toml', '--locked',
  '--format-version', '1', '--filter-platform', 'x86_64-pc-windows-msvc',
]));
const selected = new Set(metadata.resolve.nodes.map(node => node.id));
const packages = metadata.packages.filter(pkg => selected.has(pkg.id) && pkg.name !== 'maxa-device')
  .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
const stage = mkdtempSync(join(tmpdir(), 'maxa-device-sources-'));
const texts = ['Maxa Device — third-party notices', '',
  'This file preserves dependency license and attribution texts. Each component retains its own license.',
  'Matching source files are provided in third-party-sources.tar.gz alongside the installer.', ''];
const inventory = [];
const supplements = JSON.parse(readFileSync(join(root, 'licenses', 'rust-supplements.json'), 'utf8'));

function includeComponent(name, version, source, destination, license, repository, extraLicense) {
  if (!license) throw new Error(`Missing license declaration: ${name} ${version}`);
  cpSync(source, destination, { recursive: true, dereference: false,
    filter: path => !['.git', '.cargo-ok'].includes(basename(path)) });
  const candidates = readdirSync(source, { withFileTypes: true })
    .filter(entry => entry.isFile() && /^(?:licen[cs]e|copying|copyright|notice)(?:[._-]|$)/i.test(entry.name))
    .map(entry => join(source, entry.name));
  if (extraLicense) {
    const file = resolve(source, extraLicense);
    if (file !== source && !relative(source, file).startsWith('..') && existsSync(file)) candidates.push(file);
  }
  const unique = [...new Set(candidates)];
  const supplement = supplements[`${name}@${version}`];
  if (!unique.length && !supplement) throw new Error(`No license/notice text found: ${name} ${version}`);
  texts.push('='.repeat(72), `${name} ${version}`, `License: ${license}`,
    ...(repository ? [`Upstream: ${repository}`] : []));
  for (const file of unique) texts.push(`--- ${relative(source, file).replaceAll('\\', '/')} ---`, readFileSync(file, 'utf8'));
  if (!unique.length) {
    texts.push(`License-text provenance: ${supplement.provenance}`);
    for (const item of supplement.files) {
      const bytes = readFileSync(join(root, 'licenses', item.file));
      if (createHash('sha256').update(bytes).digest('hex') !== item.sha256) {
        throw new Error(`Supplemental license digest mismatch: ${item.file}`);
      }
      texts.push(`--- ${item.file} ---`, ...(item.url ? [`Source: ${item.url}`] : []), bytes.toString('utf8'));
    }
  }
  inventory.push({ name, version, license, repository: repository || null });
}

try {
  for (const pkg of packages) includeComponent(pkg.name, pkg.version, dirname(pkg.manifest_path),
    join(stage, 'rust', `${pkg.name}-${pkg.version}`), pkg.license, pkg.repository, pkg.license_file);
  for (const name of ['@tauri-apps/api', 'vite']) {
    const source = join(root, 'node_modules', ...name.split('/'));
    const pkg = JSON.parse(readFileSync(join(source, 'package.json'), 'utf8'));
    includeComponent(pkg.name, pkg.version, source, join(stage, 'javascript', name.replaceAll('/', '-')),
      pkg.license, typeof pkg.repository === 'string' ? pkg.repository : pkg.repository?.url);
  }
  // Include the modified, already-vendored helper build inputs as well as its provenance.
  cpSync(join(root, 'vendor', 'u2-server'), join(stage, 'android-helper'), { recursive: true });
  for (const name of ['Cargo.lock', 'Cargo.toml']) cpSync(join(root, 'src-tauri', name), join(stage, name));
  for (const name of ['package-lock.json', 'components.json', 'LICENSE', 'THIRD-PARTY.md']) {
    cpSync(join(root, name), join(stage, name));
  }
  cpSync(join(root, 'licenses', 'rust-supplements'), join(stage, 'license-supplements'), { recursive: true });
  cpSync(join(root, 'licenses', 'rust-supplements.json'), join(stage, 'license-supplements.json'));
  writeFileSync(join(stage, 'dependencies.json'), JSON.stringify(inventory, null, 2) + '\n');
  writeFileSync(notices, texts.join('\n') + '\n');
  cpSync(notices, join(stage, 'THIRD-PARTY-NOTICES.txt'));
  run('tar', ['-czf', join(release, 'third-party-sources.tar.gz'), '-C', stage, '.']);
  console.log(`Prepared notices and sources for ${inventory.length} dependencies.`);
} finally {
  // This directory was created by this invocation and contains only copied public sources.
  rmSync(stage, { recursive: true, force: true });
}
