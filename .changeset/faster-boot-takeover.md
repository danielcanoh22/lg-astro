---
'lg-luma': patch
---

Replace LG Home about 6 seconds sooner at cold boot: the boot hook no longer waits 5 seconds before applying the takeover, retries a failed apply up to three times, and checks every 200 ms for the previous Home to exit.
