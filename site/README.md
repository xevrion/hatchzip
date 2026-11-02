# Website

The HatchZip website: static pages, no build step. The only script is Cloudflare Web Analytics, which counts visits without cookies; the Content-Security-Policy on every page allows it and nothing else.

| File | Purpose |
|---|---|
| `index.html` | The main page |
| `sponsor.html` | Sponsoring HatchZip, for individuals and companies |
| `funding.json` | Funding manifest ([fundingjson.org](https://fundingjson.org/)) for FLOSS/fund and FOSS United; `/.well-known/funding-manifest-urls` in the repo vouches for it |
| `stats.py` | Fills in the live numbers between `<!--stat:NAME-->` markers; the pages workflow runs it every hour |
| `verify.html` | Verify your download: what HatchZip does, checksums, provenance, and the release file table |
| `update-verify.py` | Regenerates the release file table in `verify.html` |
| `not-enough-space-to-extract.html`, `winrar-not-enough-space.html`, `7zip-not-enough-space.html` | Guides for people whose extraction ran out of disk space |
| `indexnow.py`, `4114b1f4eb288ada55a869db5d3b852f.txt` | Submits the sitemap's URLs to IndexNow; the key file proves the site is ours |
| `style.css` | All styles, light and dark |
| `404.html` | Page shown for unknown addresses |
| `favicon.svg` | Browser tab icon |
| `robots.txt`, `sitemap.xml` | For search engines. `DOMAIN` is a placeholder. |
| `_headers` | Security and cache headers for Cloudflare Pages and Netlify |

## Preview locally

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory site
```

Then open http://127.0.0.1:8000. The 404 page is not served for unknown paths by this server; open `/404.html` directly.

## Before going live

- The download links point at `releases/latest/download/<file>` and the install commands at `install.sh` and `install.ps1` on `main`. Both work only once the first release and the install scripts exist.
- The site lives at `https://hatchzip.dev`, a domain verified for the `xevrion` GitHub account. That address is written in `sitemap.xml`, `robots.txt` and the canonical, `og:url` and `og:image` tags of `index.html`.
- After any change to `style.css`, bump the `?v=` number where every HTML page links it (`index.html`, `verify.html`, `404.html` and the guides). The CSS is cached for a year.

## After each deploy

Tell the IndexNow search engines (Bing, Yandex, Seznam, Naver and others) that pages changed:

```sh
python3 site/indexnow.py
```

It POSTs every URL in `sitemap.xml` to `https://api.indexnow.org/indexnow`, with the key from `4114b1f4eb288ada55a869db5d3b852f.txt`, which must be live at the site root first. A 200 or 202 answer means the URLs were received; a 403 usually means the key file is not reachable yet. `--dry-run` prints the request without sending it. When a page changes, update its `lastmod` in `sitemap.xml` before running it. Google does not use IndexNow; request indexing for new pages in Search Console instead.

## After each release

`verify.html` lists every file of one release with its SHA-256 and a VirusTotal link. Once the release is published, regenerate the table from its tag and commit the result:

```sh
python3 site/update-verify.py v0.1.4
```

Then update the version in two places by hand: `softwareVersion` in the structured data of `index.html`, and "Current version" in `llms.txt`. Set `lastmod` in `sitemap.xml` to the date of any page that changed.

`update-verify.py` needs the GitHub CLI, signed in. It downloads the release's `.sha256` files, checks each against the SHA-256 GitHub computed for the uploaded file, and rewrites only the block between the `release-files` markers. If a checksum file is malformed or disagrees with GitHub, it stops and changes nothing. The page does not quote VirusTotal results, since they change; it links to them.

## Option A: GitHub Pages

1. In the repository, open Settings, Pages, and set Source to "GitHub Actions".
2. Open the Actions tab, pick the `pages` workflow, and click "Run workflow" on `main`.
3. The site is published at `https://hatchzip.dev` (Settings, Pages, Custom domain; DNS is a `CNAME` to `xevrion.github.io` on Cloudflare, DNS only). `xevrion.github.io/hatchzip/` redirects there.

The workflow (`.github/workflows/pages.yml`) only runs when started by hand. To deploy on every push to `main` that touches the site, add this under `on:`:

```yaml
  push:
    branches: [main]
    paths: ['site/**']
```

Notes:

- GitHub Pages ignores `_headers`. The CSP and referrer policy still apply through the `<meta>` tags in the HTML, but framing protection (`frame-ancestors`, `X-Frame-Options`) and the other headers are only sent by Cloudflare Pages or Netlify.
- `404.html` links its stylesheet and home page with absolute paths (`/style.css`), which are correct at the root of the domain. The main page uses relative paths and works anywhere.

## Option B: Cloudflare Pages

Free, and it applies `_headers`. Run these from the repository root:

```sh
npx wrangler login
npx wrangler pages project create hatchzip --production-branch main
npx wrangler pages deploy site --project-name hatchzip --branch main
```

The site is published at `https://hatchzip.pages.dev` (the create command prints the exact address if that name is taken). Run the last command again to deploy a change. A custom domain is added in the Cloudflare dashboard under Workers and Pages, hatchzip, Custom domains.

`wrangler pages deploy site` uploads every file in this folder, including this README and `update-verify.py`, which then are reachable at `/README.md` and `/update-verify.py`. That is harmless, since both are public in the repository anyway.
