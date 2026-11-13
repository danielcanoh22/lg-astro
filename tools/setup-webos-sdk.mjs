/**
 * Downloads, verifies and relocates the webOS native SDK for this host (`pnpm sdk`).
 *
 * Extracts the archive pinned for the current platform and architecture into
 * `.build/toolchain` unless its compiler is already there, then runs the SDK's relocation script
 * and prints the `WEBOS_NDK` value that `tools/build-webos.sh` needs. Needs `curl` and `tar`.
 * SDL-webOS is prepared separately by `webos-runtime.mjs`.
 */
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '..');
const parent = path.join(root, '.build/toolchain');
const sdk = path.join(parent, 'arm-webos-linux-gnueabi_sdk-buildroot');
// Each archive is pinned by SHA-256: release assets can be replaced under the same tag,
// and this toolchain compiles the shipped binary.
const assets = {
  'darwin-arm64': {
    url: 'https://github.com/webosbrew/native-toolchain/releases/download/webos-d7ed7ee.6/arm-webos-linux-gnueabi_sdk-buildroot_darwin-arm64.tar.bz2',
    sha256: '954333694ab1fe1f7737e81a0be1e3188cdfffc1a119c81c86f5fcebf1857c45',
  },
  'darwin-x64': {
    url: 'https://github.com/webosbrew/native-toolchain/releases/download/webos-d7ed7ee.6/arm-webos-linux-gnueabi_sdk-buildroot_darwin-x86_64.tar.bz2',
    sha256: '75e7451ac39604872ae7f07894cbe8edb62b2091664a1781cb193e7ebe56aa75',
  },
  'linux-x64': {
    url: 'https://github.com/openlgtv/buildroot-nc4/releases/download/webos-b17b4cc/arm-webos-linux-gnueabi_sdk-buildroot.tar.gz',
    sha256: '1f69af43caac4b6898d8f0323c975be60af04abdd1e1c144773072b28c961c20',
  },
  'linux-arm64': {
    url: 'https://github.com/webosbrew/native-toolchain/releases/download/webos-d7ed7ee.6/arm-webos-linux-gnueabi_sdk-buildroot_linux-aarch64.tar.bz2',
    sha256: '45a2d12ff557457d92cde4fddaa77a6f1090fca03adc43bb74397e5e0c379501',
  },
};
const asset = assets[`${process.platform}-${process.arch}`];
if (!asset) throw new Error(`No pinned webOS SDK for ${process.platform}/${process.arch}.`);
const { url, sha256 } = asset;
if (/[\s]/.test(sdk)) throw new Error('Extract the webOS SDK in a path without spaces.');
await fs.mkdir(parent, { recursive: true });
try {
  await fs.access(path.join(sdk, 'bin/arm-webos-linux-gnueabi-gcc'));
} catch {
  const archive = path.join(parent, path.basename(url));
  const downloaded = spawnSync(
    'curl',
    ['--fail', '--location', '--retry', '3', '--silent', '--show-error', '--output', archive, url],
    { stdio: 'inherit', timeout: 300000 },
  );
  if (downloaded.error) throw downloaded.error;
  if (downloaded.status !== 0) throw new Error('webOS SDK download failed.');
  const digest = createHash('sha256');
  for await (const chunk of createReadStream(archive)) digest.update(chunk);
  const actual = digest.digest('hex');
  if (actual !== sha256) {
    await fs.rm(archive, { force: true });
    throw new Error(`webOS SDK checksum mismatch: expected ${sha256}, got ${actual}.`);
  }
  const extracted = spawnSync(
    'tar',
    [url.endsWith('.bz2') ? '-xjf' : '-xzf', archive, '-C', parent],
    { stdio: 'inherit', timeout: 120000 },
  );
  if (extracted.error) throw extracted.error;
  if (extracted.status !== 0) throw new Error('webOS SDK extraction failed.');
}
const relocated = spawnSync('sh', [path.join(sdk, 'relocate-sdk.sh')], {
  stdio: 'inherit',
  timeout: 120000,
});
if (relocated.error) throw relocated.error;
if (relocated.status !== 0) throw new Error('webOS SDK relocation failed.');
console.log(`webOS SDK ready. Set WEBOS_NDK=${sdk}`);
