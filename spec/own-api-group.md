# The network's own API group

Web requests whose API group is `anyplay` are answered by the net service in the cloud's own format. They are not a platform format: the fake game uses them to test the whole network without a real game, and a homebrew game can use them to play online. Version 0, a draft.

A request reaches the net service like any other web request of the [bridge protocol](bridge-protocol.md). Title, game version and AnyPS5 build always come from the game start the launcher reported, never from the request.

| Method and path | Body | Answer |
|---|---|---|
| `POST /v1/sessions` | `{max_members, visibility, properties}` | 200 and the cloud's `Session` |
| `POST /v1/sessions/{id}/join` | none | 200 and the `Session` |
| `POST /v1/sessions/{id}/leave` | none | 204 |
| `GET /v1/sessions/{id}` | none | 200 and the `Session` |

- `Session`, `visibility` and `properties` are those of the [cloud API](cloud-api.md). The net service adds the player's tunnel key and relay itself.
- A cloud error comes back with its HTTP status and the cloud's error body. Status 0 means the net service cannot ask the cloud: the player is signed out or offline.
- Any other path answers 404.
- Sessions a game made or joined through this group are left when the game ends.
