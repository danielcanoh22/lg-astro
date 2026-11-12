---
'lg-astro': patch
---

Keep Astro's storage folder private on TVs in developer mode. webOS creates the app's data
folder writable by every user on the TV; Astro now removes access for other users from that
folder and from its uploaded backgrounds folder at startup. Files inside are unchanged.
