# Web API group `userProfile`

Status: **observed**. The net service answers every request of this group with status 0 ("not offered") until this file says which answer the game accepts.

## Observed

| Game | Build | Run | Account |
|---|---|---|---|
| Minecraft Dungeons II | AnyPS5 with the bridge in observation mode ([anyplaynet/AnyPS5#1](https://github.com/anyplaynet/AnyPS5/pull/1)), relinked for Windows | about 130 s, ended by a GPU fault outside the network libraries | signed out: `sceNpGetAccountIdA` returned `0x80550006` |

## Requests

### `GET /v1/users/{user}/friends`

| Part | Seen |
|---|---|
| `{user}` | `local_` and the game's local user ID in decimal: `local_268435456` for the first user (`0x10000000`) |
| Query | empty: the path ends in `?` |
| Headers | one, `X-Psn-NpCppWebApi-Context` |
| Body | none |
| How often | 4 times: the first 105 s after the start, then every 5 to 10 s |
| Calls | `sceNpWebApi2CreateUserContext` once, then for each request `sceNpWebApi2CreateRequest`, `sceNpWebApi2AddHttpRequestHeader`, `sceNpWebApi2SendRequest` and `sceNpWebApi2DeleteRequest` |

## What the game did

- **"Not offered"** (`sceNpWebApi2SendRequest` returned `0x80553406`): the game deleted the request and asked again 5 to 10 s later. The run ended for another reason before anything else could be seen.

## Unknown

- The answer the game accepts: status, headers and body fields. No answer has been observed.
- Whether a signed-in player sends the same request.
- The header's value, and whether an answer has to carry it back.
- Other paths of the group, such as paging parameters or the profiles of other players.

## Next

1. A run signed in to an AnyPlay cloud with the bridge on, to see what a signed-in player sends.
2. The answer: from public documentation or other open implementations (ideas only, never their code), tried in observation runs on games we own, with what the game does written down here. Nothing is offered before this file says what the game accepts.
