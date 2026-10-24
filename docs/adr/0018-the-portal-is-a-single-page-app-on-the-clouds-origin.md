# The portal is a single-page app on the cloud's origin

The web portal (profile, friends, trophies, account) is a single-page app, built to static files, that the cloud serves from the same origin as the account web. It talks to the cloud with `fetch` on the browser session cookie and a CSRF token, through a JSON API under `/web/api/v1` ([spec](../../spec/portal-api.md)). There is no server between them: the cloud is the only server, and the only secrets in the browser are the session cookie (`HttpOnly`) and a CSRF token. Server-rendered pages, which the first research on the account web recommended, cannot carry the interaction the portal needs (live friend lists, dialogs, optimistic updates), and a second server that renders and forwards would hold the session next to the cloud and double what must be secured.

The account web keeps its HTML pages until the portal covers every flow. Both use the same session and the same steps of the sign-in flow, so they cannot disagree. `/web/authorize` and `/web/link` stay server routes: the launcher opens them. A public profile is a route of the app, but its shell carries Open Graph tags from the profile, so a shared link previews without running the app.

The content security policy stays as strict as for the HTML pages (`script-src 'self'`, `style-src 'self'`). Every write needs the CSRF token, the cloud's own origin, and `Content-Type: application/json`, which a form on another site cannot send. Actions that would give a thief of a session a lasting hold or let them read everything (passkeys, the export, a new address (by passkey, when the account has one), ending other sessions and devices, signing out everywhere, deleting the account) need a step-up. Every way to delete an account, in the portal, in the net service's API and on the HTML pages, waits 30 days and can be cancelled until the time.

## Considered options

- **Server-rendered pages with htmx and little script,** as the first research recommended: fewer moving parts and no build, but the portal's interaction would grow past it, and the team writes the interface in React.
- **A backend for the frontend (a Node server that renders and calls the cloud):** server-side rendering of profiles, but a second deployed service that must hold sessions or tokens. Link previews need only the tags, which the cloud injects.
- **Bearer tokens in the browser** (the device API with a token kept in memory or storage): a script can read them, and a cross-site script would have what an `HttpOnly` cookie hides.
