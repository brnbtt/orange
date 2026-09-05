# Orange website

Live: **https://orangealpha0d8d5893e69a3.z15.web.core.windows.net/**

Plain HTML, CSS, and a small script. The desktop logo in `assets/logo.png` is
copied into the website at deployment. `scene.svg` is an original illustration,
not an application screenshot. No build step or production dependencies.

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

The script checks all six source assets, enables static hosting, discovers the
web endpoint, and adds a Blob-service GET/HEAD CORS rule for that origin if one
does not already allow reads. Existing CORS rules are preserved. Only six
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
text, not HTML. New app releases require **no website deployment**.

The publisher already uploads and verifies the installer before updating that
manifest. The website does not keep a second release pointer. If JavaScript is
disabled, the network fails, or the manifest is invalid, links still reach
GitHub's releases page. `/releases/latest` is intentionally avoided because it
excludes the prereleases that Orange currently publishes.

CORS is configured on the **Blob endpoint** serving the manifest, not on the
static website endpoint (which does not support CORS). Changing the website's
origin requires updating that allowed origin and the HTML's Open Graph URL.

## Preview and checks

Stage the public assets locally, then use any static server. For example:

```powershell
$preview = Join-Path $env:LOCALAPPDATA 'Temp\opencode\orange-site-preview'
New-Item -ItemType Directory -Force $preview | Out-Null
Copy-Item website\index.html, website\404.html, website\styles.css, website\release.js, website\scene.svg, assets\logo.png $preview
npx --yes http-server $preview -p 4173 -c-1
```

Open `http://localhost:4173`. The local origin is not permitted to read the
production manifest, so the GitHub fallback is expected locally. Do not broaden
production CORS for a preview. Node tests cover successful and rejected
manifests and network errors without contacting Azure; the PowerShell tests
replace only the Azure boundary and verify real script behavior.

Before deploying, check 320px, 375px, tablet and desktop widths, keyboard focus,
FAQ expansion, reduced motion, and the no-JavaScript fallback. After deployment,
verify the version and both download URLs in a browser against the public
manifest, and check that an unknown path returns the custom page with HTTP 404.

Reference: [Azure Storage static website hosting and pricing model](https://learn.microsoft.com/azure/storage/blobs/storage-blob-static-website).
