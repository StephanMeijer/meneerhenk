# Dashboard app

Meneer Henk's dashboard, a single-page app (#197). It reads and acts only
through the JSON API at `/dashboard/api/v1` (`docs/API.md`). `npm run build`
writes `dist/`; `crates/henk/build.rs` embeds that in the henk binary, which
serves it at `/dashboard` behind the dashboard's sign-in.

## Layout

- `src/lib/api/types.ts`: the API's types, generated from
  `crates/henk/src/dashboard/api/types.rs`. Do not edit; run
  `HENK_BLESS=1 cargo test -p henk api_types_are_current`.
- `src/lib/api/client.ts`: the only way to Henk. Reads carry the session
  cookie, actions also the CSRF token from `/me`; a 401 goes to sign-in.
- `src/lib/router.ts`: routes on the History API under `/dashboard`: the
  overview (`/`), `/runs/{id}`, `/runs/{id}/transcripts/{session}`,
  `/events`, `/events/{id}` and `/health`, the paths the server-rendered
  pages had.
- `src/lib/format.ts`: small text helpers the views share.
- `src/lib/views/`: one component per page, and the parts they share. A
  view takes its loaders as props with the client's as defaults, so tests
  give it fixtures.
- `src/lib/testing/`: rendering into jsdom and fixtures, for the tests.
- `src/App.svelte`: the shell: navigation, who is signed in, sign-out.
- `public/`: files copied as they are, such as the favicon.

## Rules

- Text from the API is other people's words (§8.3). Show it with `{...}`,
  which renders text. `{@html}`, `innerHTML`, `outerHTML` and
  `insertAdjacentHTML` are lint errors.
- The page's policy allows no inline script or style and no `data:` URLs:
  no `style=` attributes, no Svelte transitions, no inlined assets.

## Commands

```sh
npm ci --ignore-scripts   # no dependency runs code at install
npm run dev               # Vite, forwarding the API to HENK_URL
npm run check             # svelte-check, TypeScript strict
npm run lint              # ESLint
npm test                  # Vitest, in jsdom
npm run build             # dist/, for build.rs
npm audit --audit-level=high
```

## Dependencies

All are dev dependencies: the output is static files with Svelte's runtime
bundled in.

| Package | Why |
|---|---|
| `svelte` | The components, and the runtime bundled into the app |
| `@sveltejs/vite-plugin-svelte` | Compiles `.svelte` files in Vite |
| `vite` | Dev server and production build |
| `typescript` | Types, strict |
| `svelte-check` | Type-checks `.svelte` files |
| `@types/node` | Types for `vite.config.ts` |
| `eslint`, `@eslint/js` | Lint |
| `typescript-eslint` | Lint for TypeScript |
| `eslint-plugin-svelte` | Lint for Svelte, with `no-at-html-tags` |
| `vitest` | Tests |
| `jsdom` | A DOM for component tests |
