# wake.cool

The website for Wake: a home page in English and Chinese, plus the MCP and CLI docs and the changelog, rendered from the repo's own `docs/*.md` and `CHANGELOG.md`. Astro, static output, hosted on Vercel.

```bash
pnpm install
pnpm dev        # http://localhost:4321
pnpm build      # static site in dist/
```

## Where things come from

- **Brand marks and the app icon** are copied from `crates/wake/assets` by `scripts/sync-assets.mjs` on every `dev` and `build` (resized to WebP); nothing under `public/brands` is checked in.
- **The agent list** is `src/lib/agents.ts`. Add a row when Wake gains an agent; the copy never states a count, so nothing else needs updating.
- **Download links** are resolved at build time from the GitHub API: the newest release that has all six packages. If the API can't be reached, the links are built from the version in the root `Cargo.toml`. Set `GITHUB_TOKEN` in Vercel to avoid the anonymous rate limit.
- **The docs and changelog** read `../docs/mcp.md`, `../docs/cli.md` and `../CHANGELOG.md` directly. Links between the docs are rewritten to the site's pages; other repo links go to GitHub. `[Unreleased]` is left out of the changelog page. Each also exists under `/zh/` with Chinese navigation and labels; the body stays in English, with a note saying so, and links between the pages stay in the visitor's language.
- **Copy** for the home page, in both languages, is `src/lib/copy.ts`. The search demo's sessions are made up, in `src/lib/demo-data.ts`.
- **`public/og.png`** is the social card, a screenshot of the hero at 1200×630.

## Deploying

The Vercel project's Root Directory is `site`, and "Include files outside the root directory" must stay on (the build reads `../docs`, `../CHANGELOG.md`, `../Cargo.toml` and `../crates/wake/assets`). Pushes to `main` deploy automatically.

After a release, the `site` job at the end of `.github/workflows/release.yml` calls a Vercel deploy hook once all packages are uploaded, so the download buttons switch to the new version. Create the hook in Vercel (Settings → Git → Deploy Hooks, branch `main`) and store its URL as the `VERCEL_DEPLOY_HOOK` repository secret. The same hook is called once a day by `.github/workflows/site-refresh.yml`, so the GitHub star count in the header is never more than a day old.
