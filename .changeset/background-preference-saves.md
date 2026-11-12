---
'lg-astro': patch
---

Save preferences in the background. Choosing a wallpaper, changing a setting or a wallpaper
rotation no longer waits on the interface while the TV writes the preferences file to storage. A
pending save is still written before Astro exits or re-reads preferences changed by its other
copy, and a failed save is still reported.
