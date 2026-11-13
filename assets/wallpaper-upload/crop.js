/**
 * Crop geometry of the wallpaper upload page, kept free of DOM access so
 * `tests/wallpaper-upload.test.mjs` can test it directly.
 *
 * Coordinates are in the orientation-corrected source image, never in CSS pixels.
 */

/**
 * Returns the 16:9 source rectangle that fills a 1920×1080 frame.
 *
 * @param {number} width Source width.
 * @param {number} height Source height.
 * @param {number} zoom 1 covers the frame exactly; larger values crop tighter.
 * @param {number} horizontal Position of the crop within the spare width, 0 (left) to 1 (right).
 * @param {number} vertical Position within the spare height, 0 (top) to 1 (bottom).
 * @returns {{ x: number, y: number, w: number, h: number }}
 */
export function cropRect(width, height, zoom, horizontal, vertical) {
  const scale = Math.max(1920 / width, 1080 / height) * zoom;
  const w = 1920 / scale;
  const h = 1080 / scale;
  return { x: (width - w) * horizontal, y: (height - h) * vertical, w, h };
}

/**
 * Moves a crop position (0 to 1) along one axis after a drag of `delta` preview pixels, so the
 * image follows the pointer. Centres when the crop already spans the whole source.
 *
 * @param {number} position Current position.
 * @param {number} delta Pointer movement in preview pixels.
 * @param {number} sourceExtent Source size along the axis.
 * @param {number} cropExtent Crop size along the axis, in source pixels.
 * @param {number} previewExtent Preview size along the axis, in CSS pixels.
 * @returns {number} The new position, clamped to 0 to 1.
 */
export function panPosition(position, delta, sourceExtent, cropExtent, previewExtent) {
  const range = sourceExtent - cropExtent;
  return range <= 0.001
    ? 0.5
    : Math.max(0, Math.min(1, position - ((delta / previewExtent) * cropExtent) / range));
}
