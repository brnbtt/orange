# Website type

The early Orange identity boards specify **Orbitron Bold** for headings and
the tracked uppercase wordmark, and **IBM Plex Mono Regular** for body and UI
text. The website uses those faces, served from the same Azure host as the page.

Vendored, unmodified Latin WOFF2 files from Fontsource 5.3.0:

- `orbitron-latin-700.woff2`: https://cdn.jsdelivr.net/npm/@fontsource/orbitron@5.3.0/files/orbitron-latin-700-normal.woff2
- `ibm-plex-mono-latin-400.woff2`: https://cdn.jsdelivr.net/npm/@fontsource/ibm-plex-mono@5.3.0/files/ibm-plex-mono-latin-400-normal.woff2

Both use the SIL Open Font License 1.1. Their upstream notices are included in
`orbitron-OFL.txt` and `ibm-plex-mono-OFL.txt`, copied from each package's
`LICENSE`. They are deployed with the font files. No external font request or
package installation is needed to view or deploy the website.
