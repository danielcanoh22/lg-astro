---
'lg-astro': patch
---

App icons are only read when their real location is a regular file inside the TV's application folders, so a link placed in an app's folder can no longer make Astro read another file. The Home log in `/tmp` is no longer written through a link placed at its path.
