# EVB IDE landing page

Nuxt 4 and Nuxt UI, deployed to Vercel. The layout follows the sibling EVB
Viewer landing page: a short introduction and the installers beside a film of
the real app, then what it does and common questions.

Use Node 24.21.0 and pnpm 10:

```sh
pnpm --dir landing install --frozen-lockfile
pnpm --dir landing run typecheck
pnpm --dir landing test
pnpm --dir landing run build
pnpm --dir landing run dev
```

## Film

The home page plays a video of the real app, not drawn by hand:

1. `evb-visual-capture` records its states in hidden windows, in IntelliJ Light
   and Dark, at 2880×1800, and writes a manifest of the steps (pointer, clicks,
   camera framing, keys):

   ```sh
   EVB_FILM_DIR=$PWD/.devkit/films/capture cargo run -p zed --bin evb-visual-capture --features visual-tests
   ```

2. `recorder/render.mjs` renders `recorder/film.tsx`, a Remotion composition, to
   `public/films/ide/<theme>.mp4` at 1920×1200 with a poster and a still for
   visitors who prefer reduced motion, and lists them in `app/films/ide.json`:

   ```sh
   node landing/recorder/render.mjs
   ```

The captures are render inputs under `.devkit` and are not committed. The first
render downloads Remotion's headless Chrome.

## Releases

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
in `app/assets/fonts`; application credits are on the license page.

Site code is GPL-3.0-or-later, with third-party packages and fonts retaining
their own licenses. See the root license texts and NOTICE.md.
