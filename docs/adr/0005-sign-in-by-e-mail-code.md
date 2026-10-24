# Sign in by e-mail code

Accounts have no password. A device signs in with a six-digit code that the cloud sends to the account's e-mail address, and then keeps a refresh token in the operating system's credential store. A player signs in once per device, so the extra step is rare, and the cloud never stores a secret that would be worth stealing from it. The verified address that the glossary demands is a side effect of every sign-in, not a separate step. Refresh tokens are replaced on every use; a replaced token that comes back signs the device out, because only a copy can present it twice.

The sign-in screens in the launcher are replaced by the account web, and refresh tokens are bound to a key of the device: [ADR 0016](0016-accounts-are-managed-on-the-web.md). The e-mail code remains.

## Considered options

- Handle and password with an e-mail check: one more secret to store, reset and protect against stuffing, for no gain on a device that stays signed in.
- Sign-in through Discord only: no e-mail handling at all, but every account would depend on another company's service and terms. A Discord link to find friends can still come later.
