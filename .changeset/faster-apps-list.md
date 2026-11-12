---
'lg-astro': patch
---

Build the Apps page list with less work per frame: each app title is lowercased and its favorite
position looked up once per update instead of in every comparison of the sort. The order is
unchanged.
