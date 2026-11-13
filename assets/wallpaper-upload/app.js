/**
 * Wallpaper upload page, served by Astro's temporary upload server to a phone or computer on the
 * same network (Settings → Background → Add a background).
 *
 * The user picks a JPEG, PNG or WebP (at most 16 MiB and 32 megapixels), drags and zooms a 16:9
 * crop, and the page posts only that crop as a 1920×1080 JPEG to `upload`, authenticated by the
 * session token in the URL path. The TV verifies the image again and asks its owner to save or
 * discard it.
 */
import { cropRect, panPosition } from './crop.js';

const element = (id) => document.getElementById(id);
const file = element('file');
const picker = element('picker');
const editor = element('editor');
const canvas = element('preview');
const zoom = element('zoom');
const status = element('status');
let image;
// Incremented per selected file, so a slower decode of an earlier file cannot replace a newer one.
let loading = 0;
let drag;
let busy = false;
let horizontal = 0.5;
let vertical = 0.5;

function message(text, error = false) {
  status.textContent = text;
  status.classList.toggle('error', error);
}

function crop() {
  return cropRect(image.width, image.height, Number(zoom.value), horizontal, vertical);
}

/** Draws the current crop scaled to `target`: the preview, or the 1920×1080 output canvas. */
function draw(target = canvas) {
  if (!image) return;
  const { x, y, w, h } = crop();
  const context = target.getContext('2d', { alpha: false });
  context.fillStyle = '#17191c';
  context.fillRect(0, 0, target.width, target.height);
  context.drawImage(image, x, y, w, h, 0, 0, target.width, target.height);
}

file.addEventListener('change', async () => {
  const selected = file.files[0];
  if (!selected || busy) return;
  const generation = ++loading;
  message('Opening your image…');
  try {
    if (!['image/jpeg', 'image/png', 'image/webp'].includes(selected.type))
      throw new Error('Choose a JPEG, PNG or WebP image.');
    if (selected.size > 16 * 1024 * 1024) throw new Error('Choose an image smaller than 16 MiB.');
    const next = await createImageBitmap(selected, { imageOrientation: 'from-image' });
    if (generation !== loading) {
      next.close();
      return;
    }
    if (!next.width || !next.height || next.width * next.height > 32_000_000) {
      next.close();
      throw new Error('Choose an image with at most 32 megapixels.');
    }
    image?.close();
    image = next;
    element('picker-title').textContent = selected.name;
    zoom.value = '1';
    horizontal = vertical = 0.5;
    picker.hidden = true;
    editor.hidden = false;
    draw();
    message('');
    zoom.focus();
  } catch (error) {
    if (generation === loading) message(error.message || 'This image could not be opened.', true);
  }
  file.value = '';
});

zoom.addEventListener('input', () => {
  if (!busy) draw();
});
element('change').addEventListener('click', () => file.click());
element('another').addEventListener('click', () => {
  image?.close();
  image = undefined;
  file.value = '';
  file.disabled = false;
  editor.reset();
  editor.hidden = true;
  element('success').hidden = true;
  picker.hidden = false;
  element('picker-title').textContent = 'Choose an image';
  zoom.value = '1';
  horizontal = vertical = 0.5;
  drag = undefined;
  message('');
  picker.focus();
});
canvas.addEventListener('pointerdown', (event) => {
  if (!image || busy) return;
  canvas.setPointerCapture(event.pointerId);
  drag = { x: event.clientX, y: event.clientY };
});
canvas.addEventListener('pointermove', (event) => {
  if (!drag || busy) return;
  const rect = canvas.getBoundingClientRect();
  const current = crop();
  horizontal = panPosition(horizontal, event.clientX - drag.x, image.width, current.w, rect.width);
  vertical = panPosition(vertical, event.clientY - drag.y, image.height, current.h, rect.height);
  drag = { x: event.clientX, y: event.clientY };
  draw();
});
for (const event of ['pointerup', 'pointercancel', 'lostpointercapture'])
  canvas.addEventListener(event, () => {
    drag = undefined;
  });

editor.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (busy || !image) return;
  busy = true;
  for (const input of editor.querySelectorAll('input, button')) input.disabled = true;
  file.disabled = true;
  message('Preparing your background…');
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 45000);
  try {
    const output = document.createElement('canvas');
    output.width = 1920;
    output.height = 1080;
    draw(output);
    const blob = await new Promise((resolve) => output.toBlob(resolve, 'image/jpeg', 0.94));
    if (!blob) throw new Error('The image could not be prepared. Try another photo.');
    message('Sending to your TV…');
    const token = location.pathname.split('/')[1];
    const response = await fetch('upload', {
      method: 'POST',
      headers: { 'Content-Type': 'image/jpeg', 'X-Astro-Upload': token },
      body: blob,
      signal: controller.signal,
    });
    const result = await response.json();
    // A 409 asks to save or discard the previous image on the TV first.
    if (!response.ok) throw new Error(result.error || 'The TV could not receive this background.');
    editor.hidden = true;
    element('success').hidden = false;
    message('');
  } catch (error) {
    message(
      error.name === 'AbortError'
        ? 'The TV did not respond in time. Check your TV before retrying.'
        : error instanceof TypeError
          ? 'The connection was lost. Keep the TV dialog open and check your TV before retrying.'
          : error.message,
      true,
    );
  } finally {
    clearTimeout(timeout);
    busy = false;
    for (const input of editor.querySelectorAll('input, button')) input.disabled = false;
    file.disabled = false;
  }
});
