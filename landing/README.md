# EVB IDE landing page

Nuxt 4 and Nuxt UI, deployed to Vercel. Visual conventions follow the sibling
EVB Viewer landing page. The site uses the fork's own icon and real app captures.

Use Node 24.21.0 and pnpm 10:

```sh
pnpm --dir landing install --frozen-lockfile
pnpm --dir landing run typecheck
pnpm --dir landing test
pnpm --dir landing run build
pnpm --dir landing run dev
```

`GET /api/releases/latest` fetches GitHub's latest stable release and caches a
successful result for five minutes. Installers are offered only when the
release includes a corresponding source archive and SHA-256 checksums. Missing
releases show a preparation message; API failures link directly to GitHub.

Optional private runtime variable: `NUXT_GITHUB_TOKEN` raises GitHub's API
limit. Never expose it in public runtime configuration. The public
`NUXT_PUBLIC_SITE_URL` defaults to https://evb-ide.vercel.app.

The Vercel project uses `landing` as its source root. Run
`script/evb-deploy-landing` from the repository root to stage only the landing
source, exclude Git metadata and private environment files, and deploy.

The site adds no analytics. Appearance preferences use local storage. See the
privacy page for hosting and outbound GitHub links. Font licenses are shipped
in `public/fonts`; application credits are on the license page.

Site code is GPL-3.0-or-later, with third-party packages and fonts retaining
their own licenses. See the root license texts and NOTICE.md.
