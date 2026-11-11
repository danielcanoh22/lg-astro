---
'lg-astro': patch
---

The screen is laid out once per drawn frame instead of two or more times, and results from background work (app list, launches, weather, webhooks, update checks) now wake the interface immediately instead of waiting for its next idle check.
