---
'lg-astro': patch
---

Keep the wallpaper upload session usable when other devices on the network connect to it.
Requests without the session's address no longer count toward its 64-request limit, request
headers must arrive within 5 seconds, and a slow device can no longer hold up the phone or
computer that is uploading.
