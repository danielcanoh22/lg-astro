// Capture documentation from the actual native desktop renderer, not a browser.
import fs from 'node:fs/promises';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '..');
const docs = path.join(root, 'docs');
const scratch = path.join(root, '.build');
await fs.mkdir(docs, { recursive: true });
await fs.mkdir(scratch, { recursive: true });

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: 'inherit', timeout: 120000 });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with status ${result.status}`);
}

// Force the desktop feature set even after a previous all-features lint/build.
run('cargo', ['build', '--locked', '--package', 'astro', '--no-default-features']);
const binary = path.resolve(root, process.env.CARGO_TARGET_DIR ?? 'target', 'debug/astro');
const temporary = await fs.mkdtemp(path.join(scratch, 'documentation-'));
const preferences = path.join(temporary, 'preferences.json');
// Optional: an uploaded background folder copied from the TV
// (/media/developer/com.danielcanoh22.astro/wallpapers/custom-<id>), used instead of dunes.
const upload = process.env.ASTRO_SCREENSHOT_BACKGROUND;
let background = 'dunes';
if (upload) {
  const id = path.basename(path.resolve(upload));
  if (!/^custom-[0-9a-f]{32}$/.test(id))
    throw new Error('ASTRO_SCREENSHOT_BACKGROUND must name a wallpapers/custom-<id> folder');
  for (const file of ['metadata.json', 'wallpaper.jpg', 'thumbnail.jpg'])
    if (!(await fs.stat(path.join(upload, file)).catch(() => null))?.isFile())
      throw new Error(`The uploaded background has no ${file}`);
  await fs.cp(upload, path.join(temporary, 'wallpapers', id), { recursive: true });
  background = id;
}
const configuration = {
  language: 'en',
  background,
  hour_24: false,
  show_home_names: false,
  hidden: [],
  input_modes: {},
  reduced_motion: false,
};
try {
  for (const [name, script] of [
    ['home', 'home,down'],
    ['apps', 'apps,down'],
    ['settings', 'settings,down'],
    ['inputs', 'settings,down,down,down,enter,down,down,enter'],
  ]) {
    // Independent state makes screenshots stable regardless of capture order.
    await fs.writeFile(preferences, JSON.stringify(configuration));
    run(binary, [
      '--assets',
      path.join(scratch, 'assets'),
      '--preferences',
      preferences,
      '--time',
      '2026-10-22T20:41:00+02:00',
      '--script',
      script,
      '--frames',
      '90',
      '--capture-scale',
      '1',
      '--screenshot',
      path.join(docs, `${name}.png`),
    ]);
    console.log(`Native screenshot: docs/${name}.png (1920×1080)`);
  }
} finally {
  await fs.rm(temporary, { recursive: true, force: true });
}
