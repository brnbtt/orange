# Orange website

Live: **https://orangealpha0d8d5893e69a3.z15.web.core.windows.net/**

Plain HTML, CSS, and a small download script. No build step or production
dependencies. The desktop scanline mark in `assets/logo.png` is reused as-is.

## Identity and app captures

The site follows the original identity boards: tracked uppercase Orbitron,
IBM Plex Mono, charcoal and sand, orange accents, hairline grids, open corner
brackets, and registration marks. Copy describes actual actions and media flow;
avoid slogans or claims that are not supported by the app.

Orange's source-availability and licensing policy is undecided. The public site
must not describe the app as open source or advertise a source-code license.
Third-party font notices apply only to those fonts.

Fonts are self-hosted. Their sources and licenses are in `fonts/README.md`.
The six screenshots are actual GPUI client renders with staged data, not
HTML recreations of the app or images from the early concept boards. See
`screenshots/README.md` for capture provenance. Screenshots link to their full
resolution, and the page labels their demo data explicitly.
Demo content uses gaming handles and original FPS, racing, and RPG scenes.
The 1.0 walkthrough distinguishes room codes from personal friend codes and
shows request confirmation, acceptance, and joining an accepted friend's stream.

## Hosting and cost

The site uses Azure Storage static website hosting on the existing
`orangealpha0d8d5893e69a3` account in `orange-rg`, in its `$web` container.
Enabling hosting has no fixed monthly charge; blob storage, requests, and
outbound bandwidth are metered. At modest traffic, the small site should cost
very little; installer downloads will account for much more bandwidth than the
page. This is a cost model, not a quoted Azure bill.

The Azure-provided address includes HTTPS. A custom domain is not configured.
Storage does not provide HTTPS for custom domains directly; revisit Azure
Static Web Apps' Free plan if a branded domain is needed, rather than adding
a paid gateway solely for this page.

## Deploy

From the repository root, with Azure CLI authenticated to the subscription:

```powershell
.\deploy\test-website-script.ps1
node --test website/release.test.mjs
.\deploy\website.ps1
```

The script checks all fifteen source assets, fetches and validates the public
release manifest, and stages an index with that installer as its fallback. It
then enables static hosting, discovers the
web endpoint, and adds a Blob-service GET/HEAD CORS rule for that origin if one
does not already allow reads. Existing CORS rules are preserved. Only fifteen
explicit public files are uploaded, with correct MIME types and `no-cache`
revalidation; the index is uploaded last. Re-running deploy updates these files.
The script does not deploy the relay or publish an app release.

To undo a website content change, check out the previous website revision and
run the same deployment script. Do not delete the shared storage account: it
also holds installers and sessions.

## How the download stays current

`release.js` fetches the same public `releases/orange-beta.json` that installed
clients use, with `cache: 'no-store'` and an eight-second timeout. After checking
the schema, beta channel, version and exact installer URL, it updates both
download buttons, the version label, and release notes. Notes are inserted as
text, not HTML. With JavaScript enabled, new app releases require **no website
deployment**. The distribution manifest still uses the `beta` channel internally;
the website displays its actual version rather than inferring stability from it.

The publisher already uploads and verifies the installer before updating that
manifest. Public users cannot access the private repository's GitHub releases
(anonymous requests return HTTP 404). Every site deployment therefore snapshots
the public Azure installer URL into both HTML buttons. JavaScript failures leave
those immutable links working; the failure message names the saved version.
With JavaScript disabled, a note identifies the version current when the site
was deployed. Redeploy to refresh that fallback snapshot. The checked-in preview
uses the existing 1.0.0 installer; the deployed HTML is generated in a temporary
file and does not modify it.

CORS is configured on the **Blob endpoint** serving the manifest, not on the
static website endpoint (which does not support CORS). Changing the website's
origin requires updating that allowed origin and the HTML's Open Graph URL.

## Preview and checks

Stage the public assets locally, then use any static server. For example:

```powershell
$preview = Join-Path $env:LOCALAPPDATA 'Temp\opencode\orange-site-preview'
New-Item -ItemType Directory -Force $preview | Out-Null
Copy-Item website\index.html, website\404.html, website\styles.css, website\release.js, assets\logo.png $preview
New-Item -ItemType Directory -Force "$preview\fonts", "$preview\screenshots" | Out-Null
Copy-Item website\fonts\*.woff2, website\fonts\*-OFL.txt "$preview\fonts"
Copy-Item website\screenshots\*.png "$preview\screenshots"
npx --yes http-server $preview -p 4173 -c-1
```

Open `http://localhost:4173`. The local origin is not permitted to read the
production manifest, so the saved Azure installer fallback is expected locally. Do not broaden
production CORS for a preview. Node tests cover successful and rejected
manifests and network errors without contacting Azure; the PowerShell tests
replace public HTTP and Azure boundaries and verify real script behavior.

Before deploying, check 320px, 375px, tablet and desktop widths, keyboard focus,
FAQ expansion, reduced motion, and the no-JavaScript fallback. After deployment,
verify the version and both download URLs in a browser against the public
manifest, and check that an unknown path returns the custom page with HTTP 404.

Reference: [Azure Storage static website hosting and pricing model](https://learn.microsoft.com/azure/storage/blobs/storage-blob-static-website).
