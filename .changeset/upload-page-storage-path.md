---
'lg-astro': patch
---

Stop showing the TV's storage folder on the background upload page. The page no longer asks the
TV where images are stored, and the address it used for that now returns "Not found". When Astro
running as Home has to create missing folders above the preferences or uploaded backgrounds while
saving, each new folder takes the owner of the folder it is created in, as new files already did.
