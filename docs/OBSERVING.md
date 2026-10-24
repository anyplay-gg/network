# Observing games

How the network learns what a game sends and accepts, and how that becomes a specification and then code, without crossing the line [ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md) draws. The terms are in [CONTEXT.md](../CONTEXT.md).

## Three steps, kept apart

1. **Observe.** A game the player owns writes an [observation log](../spec/observation-log.md): every call of its network libraries, its arguments and what the bridge answered. The launcher's *Record Network Calls* turns it on; for a single start, `ANYPS5_NET_OBSERVE` does. The net service adds the web requests it refused.
2. **Specify.** From the logs of one or more starts, write down in `spec/` what the game sends and which answers it accepts, in your own words: fields, types, order, errors, and what is still unknown. Quote a log only as far as it shows a field.
3. **Implement.** The net service's handler for an API group is written from the specification, not from the log. Whoever implements reads the spec; the log stays on the PC of whoever observed.

## Sources, strongest first

1. **What a game sends**, read in plain text in its observation log. It settles the request: API group, path, method, headers and body.
2. **What a game accepts**: how it goes on after an answer. It shows what is enough for the game, not what the console's service would have answered.
3. **Public documentation**, such as an engine's online subsystem or a publisher's published API.
4. **Other open implementations** of the general model (RPCN for PS3, shadNet for PS4), for ideas only; their code is not copied.

Never a source: traffic to or from Sony's servers, leaked SDKs or documents, keys, certificates or tickets, and anything taken from console firmware services.

## What an observation settles

| Seen | Settles | Does not settle |
|---|---|---|
| A request in the log | Its shape: path, method, fields, their types | Which fields the platform would require |
| The game goes on after an answer | That answer is accepted | That the platform answers the same |
| The game shows an error after an answer | The answer is refused, at least in that state | Why |
| A field the game never reads | Nothing | That the field can be left out everywhere |

When sources disagree, the specification says so.

## Rules

- **Only games the player owns.** Observation logs never leave the PC on their own. Share excerpts, never whole logs, and never saves or memory.
- **Never a made-up success.** An API group without a specification is answered with status 0, "not offered", which the game handles like being offline. A handler never guesses a 200.
- **General behaviour.** A handler implements its API group as the specification describes it for every game, not a fix for one title. Title IDs appear in specifications and pull requests, never in code or test names.
- **Each handler links its specification**, and what is missing is written down in [docs/gaps.md](gaps.md).
- **Numbers, not impressions.** A claim about latency, loss or throughput comes with `anyplay-probe` output from the machines it is about.
