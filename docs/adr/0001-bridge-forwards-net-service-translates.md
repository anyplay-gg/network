# The bridge forwards, the net service translates

The code inside AnyPS5 only hands each network library call, with request bodies as opaque bytes, to the net service over loopback. The net service turns those platform-shaped calls into the cloud's own API. We chose this because AnyPS5 accepts third-party code only as submodules and has no JSON, WebSocket or ICE code, so a small forwarding bridge is the only shape it could ever take in; because fixes for a single game then ship with a launcher update instead of an AnyPS5 build or a cloud deploy; and because the cloud never learns platform formats, which keeps it clean and usable by other clients.

## Considered options

- Translating inside AnyPS5: needs a JSON stack and per-game knowledge in the engine, and every fix waits for an engine build.
- Translating in the cloud: the platform's request shapes would live on our servers, and every game fix becomes a server deploy.
