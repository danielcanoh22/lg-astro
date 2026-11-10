import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import http from 'node:http';
import { spawn } from 'node:child_process';
import messages from '../assets/messages.json' with { type: 'json' };

const root = path.resolve(import.meta.dirname, '..');
const out = path.join(root, '.build');
await fs.mkdir(out, { recursive: true });
const temporary = await fs.mkdtemp(path.join(out, 'translations-smoke-'));
const preferences = path.join(temporary, 'preferences.json');
const binary = path.resolve(root, process.env.CARGO_TARGET_DIR ?? 'target', 'debug/astro');
const requests = [];
const places = {
  en: { name: 'Vienna', region: 'Vienna', country: 'Austria' },
  es: { name: 'Viena', region: 'Viena', country: 'Austria' },
};
const server = http.createServer((request, response) => {
  const url = new URL(request.url, 'http://127.0.0.1');
  requests.push({ path: url.pathname, query: Object.fromEntries(url.searchParams) });
  response.setHeader('Content-Type', 'application/json');
  if (url.pathname === '/v1/search') {
    const place = places[url.searchParams.get('language')];
    assert.ok(place, 'Geocoding must use a supported UI language');
    response.end(
      JSON.stringify({
        results: [{ ...place, admin1: place.region, latitude: 48.2085, longitude: 16.3721 }],
      }),
    );
  } else if (url.pathname === '/v1/forecast') {
    assert.equal(url.searchParams.get('temperature_unit'), 'celsius');
    response.end(JSON.stringify({ current: { temperature_2m: 14.2, weather_code: 2, is_day: 1 } }));
  } else {
    response.writeHead(404);
    response.end('{}');
  }
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
async function run(command, args) {
  await new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd: root, stdio: 'inherit' });
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error('Translation smoke child timed out'));
    }, 45000);
    child.on('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      if (code === 0) resolve();
      else reject(new Error(`${command} exited ${code}`));
    });
  });
}
async function scenario(name, script, extra = []) {
  const dump = path.join(temporary, `${name}.json`);
  const report = path.join(out, `language-${name}-render.json`);
  await run(binary, [
    '--preferences',
    preferences,
    '--weather-test-server',
    origin,
    '--script',
    script,
    '--frames',
    '120',
    '--time',
    '2026-10-21T20:41:00+02:00',
    '--screenshot',
    path.join(out, `language-${name}.png`),
    '--dump-state',
    dump,
    '--diagnostics',
    report,
    ...extra,
  ]);
  const state = JSON.parse(await fs.readFile(dump, 'utf8'));
  const render = JSON.parse(await fs.readFile(report, 'utf8'));
  assert.equal(state.error, null);
  assert.equal(
    render.capture.missing_glyphs,
    0,
    'Translated text must not become missing-character boxes',
  );
  assert.equal(render.capture.glyph_failures, 0);
  assert.equal(
    render.capture.missing_images,
    0,
    'Settings icons and other translated-screen artwork must be present',
  );
  return state;
}
function contains(state, key, language) {
  assert.ok(state.ui_text.includes(messages[key][language]), `Missing rendered ${key}/${language}`);
}
const picker = 'settings,down,enter,down,down,enter';
const weatherPage = 'settings,down,down,down,enter,down,down,enter';
const inputsPage = 'settings,down,down,down,enter,down,down,down,down,enter';
const location = `${weatherPage},down,enter`;
try {
  if (process.env.ASTRO_SMOKE_SKIP_BUILD !== '1')
    await run('cargo', ['build', '--locked', '--package', 'astro']);
  await fs.writeFile(
    preferences,
    JSON.stringify({ language: 'en', show_home_names: true, reduced_motion: true }),
  );
  const languages = await scenario('picker-en', picker);
  assert.equal(languages.subpage, 'Language');
  assert.deepEqual(languages.focus, { Choice: 0 });
  for (const name of ['English', 'Español']) assert.ok(languages.ui_text.includes(name));

  const spanish = await scenario('es-home', `${picker},down,enter,home,down`);
  assert.equal(spanish.preferences.language, 'es');
  for (const key of [
    'home',
    'apps',
    'settings',
    'favorites',
    'inputsHeading',
    'evening',
    'setWeatherLocation',
  ])
    contains(spanish, key, 'es');
  assert.ok(spanish.ui_text.includes('MIÉRCOLES, 21 DE OCTUBRE'));
  assert.ok(spanish.ui_text.includes('Netflix'), 'Installed app names stay intact');
  const persisted = await scenario('es-settings', 'settings,down');
  assert.equal(persisted.preferences.language, 'es', 'Spanish is restored after restart');
  for (const key of ['language', 'appNames', 'clock']) contains(persisted, key, 'es');

  const selected = await scenario(
    'es-weather',
    `${location},type:Viena,back,enter,await-search,down,enter,home,await-weather`,
  );
  assert.equal(selected.preferences.weather_location.name, 'Viena');
  contains(selected, 'partlyCloudy', 'es');
  assert.equal(
    requests.filter((request) => request.path === '/v1/search').at(-1).query.language,
    'es',
    'Geocoding must use the Spanish locale',
  );
  const forecasts = requests.filter((request) => request.path === '/v1/forecast').length;
  const changed = await scenario('en-weather', `${picker},up,enter,home,await-weather`);
  assert.equal(changed.preferences.language, 'en');
  contains(changed, 'partlyCloudy', 'en');
  assert.ok(changed.ui_text.includes('Viena'), 'The saved city name is data, not a UI label');
  assert.equal(
    requests.filter((request) => request.path === '/v1/forecast').length,
    forecasts,
    'Language changes reuse weather cache',
  );
  const englishPlace = await scenario(
    'en-search',
    `${location},type:Vienna,back,enter,await-search,down,enter`,
  );
  assert.equal(englishPlace.preferences.weather_location.name, 'Vienna');
  assert.equal(
    requests.filter((request) => request.path === '/v1/search').at(-1).query.language,
    'en',
  );

  const saved = JSON.parse(await fs.readFile(preferences, 'utf8'));
  await fs.writeFile(preferences, JSON.stringify({ ...saved, language: 'es' }));
  const dialog = await scenario('es-dialog', 'apps,down,menu');
  for (const key of ['removeFavorite', 'hideApp', 'categoryStreaming']) contains(dialog, key, 'es');
  const input = await scenario('es-input-options', `${inputsPage},enter`);
  for (const key of ['stateOn', 'stateOff', 'stateAuto', 'inputWhenConnected'])
    contains(input, key, 'es');
  const background = await scenario('es-background', 'settings,down,down,enter');
  for (const key of ['background', 'uploadWallpaper']) contains(background, key, 'es');
  assert.deepEqual(background.focus, { Background: 3 });
  // Background thumbnails have no captions; their actions are translated in the popup.
  const backgroundOptions = await scenario(
    'es-background-options',
    'settings,down,down,enter,down,enter',
  );
  assert.equal(backgroundOptions.modal, true);
  assert.equal(backgroundOptions.focus, 'UseBackground');
  for (const key of ['useBackground', 'deleteBackground']) contains(backgroundOptions, key, 'es');
  const options = await scenario('es-app-options', 'apps,down,enter');
  for (const key of ['open', 'removeFavorite', 'hideApp', 'deleteApp'])
    contains(options, key, 'es');
  const deleteConfirmation = 'apps,down,enter,down,down,down,enter';
  const confirmation = await scenario('es-delete-confirm', deleteConfirmation);
  assert.equal(confirmation.focus, 'CancelDelete');
  contains(confirmation, 'cancel', 'es');
  contains(confirmation, 'deleteApp', 'es');
  assert.ok(
    confirmation.ui_text.includes(messages.deleteAppWarning.es.replace('{app}', 'Netflix')),
    'The deletion warning must be Spanish and preserve the installed app name',
  );
  const cancelled = await scenario('es-delete-cancel', `${deleteConfirmation},enter`);
  assert.deepEqual(cancelled.focus, { Option: 2 });
  assert.ok(cancelled.apps.includes('netflix'), 'Cancel must not delete the app');
  const keyboard = await scenario('es-keyboard', 'apps,type:Netflix');
  for (const key of ['findApp', 'space', 'delete', 'clear', 'done']) contains(keyboard, key, 'es');
  const weatherSettings = await scenario('es-weather-settings', weatherPage);
  for (const key of ['widgets', 'showWidget', 'location', 'temperature'])
    contains(weatherSettings, key, 'es');
  assert.ok(
    !weatherSettings.ui_text.some((label) => label.includes('Open-Meteo')),
    'Weather credit is not shown in the UI',
  );
  await scenario('es-4k', 'home,down', ['--capture-scale', '2']);
  console.log(
    'Native translation checks passed: English and Spanish selection, saved language, shared labels, Spanish date formatting and accented glyphs, deletion/cancel UI, localized geocoding/weather, unchanged app names and 4K rendering.',
  );
} finally {
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
  await fs.rm(temporary, { recursive: true, force: true });
}
