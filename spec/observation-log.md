# Observation log

The local record of the network library calls a game makes and what it got back. The network's own specifications of what games send and accept are written from it ([ground rule 4](../docs/DESIGN.md#ground-rules)). Version 1.

An observation log never leaves the player's PC on its own. Nothing uploads it; a player who wants to help shares it by hand, and only what they choose.

## Who writes it

- **The bridge**, when `ANYPS5_NET_OBSERVE` names a file. It appends one line per call of a network library export, whether or not a net service is connected. The launcher sets the variable when the player turns on recording for a game.
- **The net service**, for every web request it refused, in `observations/<title ID>.jsonl` in its data folder.

## Lines

Each line is one JSON object in UTF-8, ended by `\n`. A writer that falls behind drops lines rather than slow the game, and counts them in a `dropped` line.

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `t_ns` | number | Nanoseconds since the writer started. |
| `src` | string | `bridge` or `service`. |
| `lib` | string | The library, such as `libSceNpManager`. Bridge lines only. |
| `fn` | string | The export, such as `sceNpGetState`, or for the net service what it handled (`web`). |
| `in` | object | The arguments that matter, by name. Pointers are left out; what they point to is written out where the export reads it. |
| `out` | object | What the export wrote back to the game, by name. |
| `ret` | string | The return value in hex, such as `0x80550006`, for exports that return a code. |
| `dropped` | number | Only on a line of its own (`fn` is `dropped`): how many lines were lost since the last one. |

Two lines describe the writer rather than a call. Both carry `lib` `libSceNetCtl`, the library the bridge lives in:

- **`dropped`**, with the field `dropped`: how many lines were lost since the last one.
- **`not_implemented`**, with `in.export`: the name of an export the game called that AnyPS5 does not implement. The bridge writes it straight to disk, after the lines still queued, just before that export stops the game, so the last call a game made is never lost. It has no `ret`.

A reader skips a line whose `fn` it does not know.

Strings from the game are written as JSON strings when they are valid UTF-8 and as `{"b64": "..."}` otherwise. A body longer than 64 KiB is cut there and the line gets `"cut": <full length>`.

Example lines:

```json
{"v":1,"t_ns":812345,"src":"bridge","lib":"libSceNpManager","fn":"sceNpGetState","in":{"user_id":268435456},"out":{"state":2},"ret":"0x0"}
{"v":1,"t_ns":901100,"src":"bridge","lib":"libSceNpWebApi2","fn":"sceNpWebApi2SendRequest","in":{"api_group":"example","path":"/v1/items","method":"GET","headers":{},"body":""},"out":{"status":0},"ret":"0x80553406"}
{"v":1,"t_ns":955000,"src":"bridge","lib":"libSceNetCtl","fn":"not_implemented","in":{"export":"sceNpExampleCall"},"out":{}}
```
