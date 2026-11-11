---
'lg-astro': patch
---

App icons and app manifests are only read when their real location is a regular file inside the TV's application folders, so a link placed in an app's folder can no longer make Astro read another file. The Home log in `/tmp` is recreated on every start instead of being written through a link or file placed at its path.
