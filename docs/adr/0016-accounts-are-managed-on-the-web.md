# Accounts are managed on the web; the launcher only starts the browser flow

Signing in, creating an account and managing it (e-mail address, passkeys, devices, a copy of the data, deletion) happen on pages of the cloud, in the player's own browser. The launcher has two buttons, "Sign in" and "Create account"; each asks the net service for a browser sign-in and opens the address it answers. Why: one place for e-mail, passkeys, recovery and, later, Apple, Google and Discord sign-in; WebAuthn needs a web origin, which a native window has not; less security-sensitive interface (codes, recovery, deletion) in the launcher; and the web portal for profiles, trophies and friends comes next and grows on the same site. The commands `email`, `code` and `sign_up` stay, deprecated, until the launcher has switched. This replaces the sign-in screens of [ADR 0005](0005-sign-in-by-e-mail-code.md); the e-mail code itself stays as the way to sign in, to recover and to verify an address.

This record is also the design of the device binding that goes with it. The aim: a stolen token is useless elsewhere, and "sign out everywhere" works.

## Browser flow (RFC 8252)

1. The net service listens on `127.0.0.1` on a random port and makes a PKCE verifier (RFC 7636) and a `state`. It answers the launcher `<web>/web/authorize?...` with the S256 challenge, the state, `redirect_uri=http://127.0.0.1:<port>/callback` and its device name. `<web>` is `web` in `GET /v1/info`: the cloud's `public_url`, and the origin of passkeys.
2. The cloud accepts a redirect only to `127.0.0.1` or `[::1]`, `http`, a port from 1024, a plain path. Anything else is refused before any page shows. The request lives in a server-side session, not in later URLs.
3. The player signs in by passkey or e-mail code, or creates the account (address, code, handle, age, terms). The browser session on these pages is its own cookie (`HttpOnly`, `SameSite=Strict`, `Secure` on https, `__Host-` prefix), short-lived, with a CSRF token on every form, apart from device tokens.
4. The cloud sends the browser to the loopback address with a one-time code (60 seconds) and the `state`. The net service checks the state and sends `{code, verifier, device}` to `POST /v1/auth/browser/token`. A wrong verifier, a used or an old code fails; the first try uses the code up.

`account_page` is a short link from the net service: the request is signed by the device key, and the link (60 seconds, single use) only asks "Open account?" on the web, so a preview cannot use it.

## Tokens

- Access token `v2.<payload>.<signature>` (Ed25519): payload `k` (kind), `aud` (`cloud` or `relay`), `kid` (first 4 bytes of the SHA-256 of the public key, in hex), `a`, `d` or `e`, `i` (issued), `x` (expires). The signature covers `anyplay-ticket-v2\0` and the token text, so nothing else signed with the key is a token. A `kid` this cloud does not have is refused, which lets keys rotate. Tokens of version 1 are still read, with `i` = 0, and no longer made. Test vectors are in `crates/ticket`.
- Refresh tokens are random, kept as SHA-256, replaced on every use.
- Revoking stateless tokens: an access token lives 15 minutes and its device must exist, which every request checks (a signed-out device is gone at once). For the rest there is one "not before" time per account: tokens issued before it are refused. Relay tickets (24 hours) stay valid until they end; the relay does not call the cloud.

## Device key

Each sign-in makes an Ed25519 key on the PC. The net service keeps its secret in the entry that holds the refresh token (the system's credential store, or an owner-only file): same place, same protection. The public key goes to the cloud with the device at sign-in. There is one key per cloud and sign-in, so keys do not link accounts or clouds.

## Proofs

A proof is the header `AnyPlay-Proof: v1.<time>.<nonce>.<signature>`: a signature over a domain string, method, path, time, a random nonce and the SHA-256 of the token the request carries. The cloud accepts it within 60 seconds and once (the nonce is kept until then, in the store, so every node refuses a replay).

- **Refresh requires it** for every device that has a key. A copied refresh token alone fails; it does not sign the device out, so a copy cannot be used to sign someone out.
- **Account link requires it** (`browser/link`): a stolen access token cannot open the account web.
- **Other requests do not carry one.** Proving every request costs a signature and a store write per request and covers only the 15 minutes of an access token, and it stops copies from logs and proxies, not malware as the user, which can sign too. Refresh is where a token becomes long-lived, so that is where the key is checked. A later version can require it for more.

## Reuse and families

The refresh tokens of a device form one family. A replaced token that comes back, with or without a proof, signs the device out and ends the family: only a copy can present it. A rotation that two requests race for ends it too.

## Devices that have no key

Devices signed in before this ADR have none. The migration gives each 30 days (`legacy_until`) in which they refresh as before; a refresh that brings a public key and a proof made with it binds the device, and the net service does that on its first refresh. After the 30 days, or in a cloud with `legacy_device_days = 0`, such a device must sign in again. A device that is bound is never unbound.

## Passkeys

webauthn-rs 0.5.5 (MPL-2.0, no independent audit; it links OpenSSL), on the cloud's origin only, user verification required, no attestation. Registration asks for a discoverable credential, so sign-in needs no address: the authenticator names the account in the user handle, which must be the one derived for the account that owns the credential. State of a ceremony is kept in the store and taken once. A counter that does not rise is refused. The e-mail code stays as recovery; removing the last passkey is allowed.

## Other providers

Apple, Google and Discord would be one more button on the sign-in page, answered by an OpenID Connect redirect to the cloud, which ends in the same step: a verified identity, an account found or created, a one-time code to the loopback address. Nothing in the launcher or the net service changes.

## What it stops

Stolen logs, proxy captures and copied files of a refresh token or an access token; token reuse after a theft; a look-alike site collecting a passkey sign-in (a passkey only answers on the real origin). Not malware that runs as the player: it can ask the net service, or use the key. Not a stolen PC that is unlocked.

## Considered options

- DPoP (RFC 9449) on every request: the same protection against copies, a JWT parser in the path of every request, and the replay store written on each one.
- A proof only at refresh with a long access token: fewer refreshes, a worse window. The 15 minutes stay.
- Passkeys in the launcher's own window: no platform gives a native window a web origin.
