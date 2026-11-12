---
'lg-astro': patch
---

Make wallpaper transitions smoother. When a newly chosen or rotated wallpaper fades in, both
wallpapers and any full-screen dimming above them are now drawn in a single pass instead of one
pass per layer, so the transition needs less graphics work per frame and looks the same as
before.
