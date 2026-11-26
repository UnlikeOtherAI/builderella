# Builderella dashboard

The web UI of a Builderella controller: a static single-page app (React,
TypeScript, Vite) with six sections and a sign-in screen. The controller
embeds the built files and serves them at `/`; there is no frontend server.

Every page leads with a summary and keeps the detail one click away:

* **Overview** (`/`): what needs a person, five headline numbers, the
  machines one line each, what runs and what waits next, recent activity.
* **Machines** (`/machines`, `/machines/<id>`): one table to compare them;
  a machine's page has its load broken down, what runs on it, health,
  policy and hardware.
* **Jobs** (`/jobs`, `/jobs/<id>`): one table with filters and search; a
  job's page has its status and Cancel on top and tabs for the summary,
  steps, log, artifacts and details.
* **Queue** (`/queue`): waiting jobs one line each with why they wait,
  expandable to the per-machine explanation; jobs in progress; fair share.
* **Services** (`/services`, `/services/<id>`): what the API calls
  environments: endpoints, time left, Renew and Finish.
* **Settings** (`/settings`): machine policies, adding a machine, projects
  and access tokens.

Machines and jobs share one status vocabulary (`src/lib/status.ts`): a
coloured dot and a word, with the precise state in a tooltip and on the
detail page. The routes of earlier versions (`/fleet`, `/environments`,
`/admin`) redirect to their new names; `/jobs/<id>`, which the CLI and the
MCP server print, never moves.

It reads the REST API under `/api/v1` and follows the event stream at
`/api/v1/events`. The wire types are the Rust types in
`crates/bella-core/src`; `src/api/schemas.ts` mirrors them as zod schemas,
and every response is parsed with one before anything is drawn. A response
of the wrong shape is shown as an error, never as data.

## Commands

Run these in `dashboard/` with Node 24 or newer.

```sh
npm ci                 # install exactly what package-lock.json names
npm run dev            # dev server; /api is proxied to a controller
npm run typecheck      # tsc, no output
npm test -- --run      # unit and component tests, once
npm run build          # static files into dist/
npm run preview        # serve dist/ to check the build
```

`npm run dev` and `npm run preview` refuse requests for a host name they do
not know. To open either from another machine by name, start it with
`--host 0.0.0.0` and `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS=<that-name>`.

`npm run dev` forwards `/api` to `http://127.0.0.1:7420`. To use another
controller:

```sh
BELLA_CONTROLLER_URL=https://controller.example:7420 npm run dev
```

## Mock mode

Without a controller, add `?mock=1` to the URL, or build with `VITE_MOCK=1`:

```sh
npm run dev            # then open http://localhost:5173/?mock=1
VITE_MOCK=1 npm run build
```

Mock mode answers from `src/mocks/fixtures.json` and shows a "Mock data"
badge. Writes (cancel, renew, policy edits, pairing codes, tokens) change an
in-memory copy and are lost on reload. `?mock=1&role=viewer` shows what a
viewer sees (no Cancel, Renew or Finish, read-only Settings). Nothing in
mock mode moves on its own; it is not a model of the scheduler.

## Fixtures

`src/mocks/fixtures.json` is printed by a Rust example that builds sample
values from the real `bella_core::api` types, so the samples cannot drift
from the wire format unnoticed. After changing a serialized type, regenerate
it from the repository root and rerun the tests:

```sh
cargo run -q -p bella-core --example api_fixtures > dashboard/src/mocks/fixtures.json
```

The schema tests parse the whole file and fail when a shape no longer
matches.

## Layout

```
src/api/         schemas, fetch layer, event stream, query hooks, mock layer
src/lib/         status vocabulary, formatting, load-bar arithmetic, lease and policy maths, labels
src/components/  frame (sidebar, top bar), icons, load bars, queue reason, job result, log viewer, UI kit
src/views/       the pages and sign-in
src/mocks/       fixtures.json (generated)
src/test/        test helpers: fixture copies, rendering the app against the mock
```

## Notes for whoever wires this to the controller

* Routing uses the URL fragment (`/#/queue`), so the controller only has to
  serve `index.html` at `/` and the files under `assets/`.
* The first event-stream request asks for recent history, so the activity
  feed does not start empty: `after` is 300 below the newest `last_event`
  among the latest 100 jobs (`0` without jobs). If the job list cannot be
  read it sends no `after` and gets only new events. Reconnects send
  `after=<last sequence seen>`.
* The bearer token is kept in `localStorage` and removed on sign-out or when
  the controller answers 401.
