---
'lg-astro': patch
---

Stop checking for the old weather forecast cache at every start. Astro 4.0.0 and later already
deleted `preferences.weather.json` once; this version no longer looks for it.
