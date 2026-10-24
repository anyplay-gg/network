# Share play: prior art and sources

What we looked at before designing [share play](../spec/share-play.md), and what we took from it. We take ideas only. No code, protocol text or key from any program below is used ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)). Read in October 2026; pages change.

## Programs and features

### Steam Remote Play Together

The host owns and runs the game. Steam encodes video, audio and voice and sends them to guests; each guest's controller or keyboard input comes back. Only the host needs the game. The host decides per guest whether input is controller only or also keyboard and mouse.

- Sources: [Steam Remote Play](https://store.steampowered.com/remoteplay), [Steamworks documentation](https://partner.steamgames.com/doc/features/remoteplay).
- Taken: the host-owns-the-game model, per-guest input permission, one invitation from the friends list.
- Not taken: it streams the desktop and injects input into the operating system. We do neither ([why](../spec/share-play.md#input)).

### Parsec

Peer-to-peer game streaming with its own UDP protocol (BUD, encrypted with DTLS 1.2, with its own congestion control), hardware encoding and decoding, H.264 and H.265. Guests have controller permission by default and the host grants more. Its help gives the latency numbers we use as a yardstick: about 7 ms of added input delay on a LAN, encode and decode each below 15 ms at 60 frames per second, and encode times that differ by vendor (NVIDIA's median was 5.8 ms and AMD's older VCE 15.1 ms in an old post; Intel was about 1.9 times NVIDIA's).

- Sources: [technology](https://parsec.app/technology), [hosting and permissions](https://support.parsec.app/hc/en-us/articles/32381747079572-Hosting-and-Permissions), [BUD](https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007), [encoder latency](https://parsec.app/blog/nvidia-nvenc-outperforms-amd-vce-on-h-264-encoding-latency-in-parsec-co-op-sessions-713b9e1e048a), [troubleshooting](https://support.parsec.app/hc/en-us/articles/32381352822804-Troubleshooting-Lag-Latency-and-Quality-Issues), [input latency test](https://parsec.app/blog/testing-game-streaming-input-latency-on-parsec-with-diy-instructions-49ae838f45a7).
- Taken: controller-only guests by default, hardware encode with a visible warning when it falls back to software, the camera method for measuring latency, the 15 ms warning level.
- Not taken: the protocol; Parsec is proprietary.

### Moonlight and Sunshine

Open-source client and host for the GameStream-style protocol: RTP-like video over UDP with Reed-Solomon loss protection and a request for a new frame after loss, control over ENet, input over the control channel, hardware encoders for NVIDIA, AMD and Intel and software fallback. Both are GPL-3.0.

- Sources: [Sunshine](https://github.com/LizardByte/Sunshine), [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c), [protocol notes by another project](https://games-on-whales.github.io/wolf/stable/protocols/index.html).
- Taken, as ideas: Reed-Solomon over a frame's packets, a systematic code, key frame on request, a hardware-first encoder list.
- Not taken: any code, constant or packet layout. The GPL would also bind the worker. Our formats are in the spec and were written without reading theirs line by line.

### The share-play feature of current consoles

Described only from the maker's public help pages. A host in a party invites one friend; the visitor sees the host's screen and, by the host's choice, either plays as the host or plays beside the host as a second player. The host can take control back. One visitor at a time; a session ends after an hour (one page says 60 minutes, another one hour) and can be started again; on the current console the host needs a subscription; the pages recommend about 2 Mbit/s each way; "both of you can enjoy the same game, regardless of who owns it". A separate screen-share mode only shows the screen.

- Sources: the console maker's public help page for the current console and the user manual of the older one (links kept in the project's private notes, so that no platform name appears in this repository).
- Taken: three modes (watch, hand over, play together), party as the scope, one guest, the host can always take control back, consent by invitation.
- Not taken: the hour limit (the operator may set one), the subscription, any format.

### Remote play of one's own console

Streaming one's own console to one's own device, as current consoles offer. Different problem (one account on both ends), but useful for bit rates: one maker's page asks for 10 Mbit/s down. Source: that maker's public remote play page (link in the private notes).

## Codecs and licences

| Subject | What we found | Source |
|---|---|---|
| H.264 pool | Via LA licenses the pool. Products that encode or decode pay per unit, the first 100,000 units a year at no cost, and the licence includes end users' personal and no-remuneration use. | [Via LA AVC/H.264](https://www.via-la.com/licensing-programs/avc-h-264) |
| OpenH264 | Cisco's source is BSD. Cisco's binary is covered by its own pool licence at no cost if the binary is downloaded separately to the end user's device, not combined with other software before, and the user can enable and disable it. Cisco gives no promise about patents beyond the pool royalty. | [binary licence](https://www.openh264.org/BINARY_LICENSE.txt), [FAQ](https://www.openh264.org/faq.html) |
| HEVC | More than one pool: Access Advance (HEVC Advance), Via LA / VCL Advance, and others. Royalty depends on where a product is made and sold. | [Via LA HEVC/VVC](https://www.via-la.com/licensing-programs/hevc-vvc), [Access Advance](https://accessadvance.com/topic-where-and-when-is-a-royalty-due) |
| AV1 | The Alliance for Open Media licenses its members' patents royalty-free under its Patent License 1.0. Sisvel runs a pool for patents of non-members, says it has licensed about half the AV1 product market (2025), and says it lists over 1,900 patents. The Alliance disputes the need for it. Unresolved. | [AOMedia licence text](https://aomedia.googlesource.com/aom/+/d38017d8878e02a781479688f5bc7bbfb47cbe34/third_party/SVT-AV1/PATENTS.md), [Sisvel](https://www.sisvel.com/licensing-programmes/audio-and-video-coding-decoding/video-coding-platform-av1), [Sisvel 2025](https://www.sisvel.com/insights/sisvel-has-licensed-half-the-av1-market-while-bringing-much-needed), [2019 report](https://www.mux.com/blog/did-sisvel-just-catch-aom-with-their-patents-down) |
| AV1 hardware | Encoders on NVIDIA RTX 40 and 50, AMD RDNA 3 and 4, Intel Arc and Core Ultra; decoders on most GPUs since 2020; no Apple chip encodes it. | [NVIDIA](https://blogs.nvidia.com/blog/av1-obs29-youtube/), [2026 overview](https://www.forasoft.com/learn/video-encoding/articles/av1-state-2026) |
| GPU encoder licences | AMD's AMF is MIT, and its licence says AMD gives no licence to any codec's patents and that royalties owed to third parties are the user's to pay. Intel's Video Processing Library runtime is MIT. NVIDIA's SDK has its own agreement; the open `nv-codec-headers` that FFmpeg uses load the driver at run time. | [AMF licence](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/LICENSE.txt), [Intel runtime](https://github.com/intel/vpl-gpu-rt), [NVIDIA agreement](https://developer.nvidia.com/nvidia-video-codec-sdk-license-agreement) |
| Linux decoders | Some distributions ship Mesa without H.264 and H.265 hardware decoding for patent reasons; free codecs (VP9, AV1) work out of the box. | [Fedora](https://fedoraproject.org/wiki/Hardware_Video_Acceleration) |
| Vulkan Video | Khronos has encode extensions, including one for AV1. Driver coverage was not measured here. | [VK_KHR_video_encode_av1](https://github.khronos.org/Vulkan-Site/features/latest/features/proposals/VK_KHR_video_encode_av1.html) |

What this means, shortly. H.264 is the only codec every PC handles in hardware, and the pool's terms are mild for personal use, but a distributor's position is not settled by a GPU vendor's licence. AV1 is free by a licence that covers its members and is challenged by a pool for others. HEVC buys little we need and has the most pools. That is why the design prefers AV1, falls back to H.264, and asks for a legal review before a public cloud turns the feature on. This is not a legal opinion.

## Transport and congestion control

- **QUIC datagrams.** [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221) adds unreliable datagrams to QUIC. They are governed by the connection's congestion controller: the sender must either wait or drop what the controller does not allow. iroh exposes `send_datagram` and `max_datagram_size`; its `send_datagram_wait` waits for buffer space, which favours old data over new, wrong for video ([iroh docs](https://docs.rs/iroh/latest/iroh/endpoint/struct.Connection.html)).
- **What goes wrong in practice.** A quinn user who sent a 4 Mbit/s datagram stream saw application-layer loss without any network loss, because the controller allowed less than the encoder produced; the answer was a bigger send buffer or a lower rate ([quinn issue 2156](https://github.com/quinn-rs/quinn/issues/2156)). Hence our rule that the encoder follows the connection.
- **Real-time media over QUIC.** The IETF's [RTP over QUIC draft](https://www.ietf.org/archive/id/draft-ietf-avtcore-rtp-over-quic-04.html) names SCReAM ([RFC 8298](https://www.rfc-editor.org/rfc/rfc8298)) and NADA as delay-based controllers for the media and QUIC's own controller beside it. A study of that nesting found that SCReAM with NewReno underneath works ([Ott and others, 2021](https://www.netlab.tkk.fi/~jo/papers/epiq21-rtp-over-quic.pdf)). [BBR](https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-04.html) is the other candidate for the QUIC side.
- **Media over QUIC** sends a group of pictures on a stream and resets it when late, using datagrams only for what is too time-sensitive to wait ([overview](https://www.forasoft.com/learn/video-streaming/articles-streaming/media-over-quic-moq)). It is our fallback if datagrams fail in phase 0.
- **iroh.** Paths are chosen by the endpoint, relays are the fallback, and iroh is moving to QUIC multipath ([post](https://www.iroh.computer/blog/iroh-on-QUIC-multipath)); path switching during a share may therefore change the round trip, and the rate controller must tolerate it.
- **Loss protection.** Moonlight uses Reed-Solomon per frame. Intra refresh and key frames on loss are compared in [Prism](https://www.cs.cmu.edu/~srini/papers/papers/2022-Ray-multimedia/2022-Ray-multimedia.pdf), which splits bandwidth between a low-delay key-frame stream and a high-quality stream during loss; we do not take that. Gradual decoder refresh is a feature of the encoder ([an example](https://docs.amd.com/r/en-US/pg252-vcu/GDR-Intra-Refresh)).

## Latency budgets

- A good local setup spends about 1 to 4 ms to capture, 5 to 9 ms to encode in hardware, 1 to 2 ms on a wired network, a few ms to decode, plus the display ([a 2026 summary](https://www.bigiron.cc/guides/the-latency-budget-for-game-streaming-encode-network-decode)). Parsec measured 4 to 8 ms for the whole pipeline at 240 frames per second on a LAN, and 7 ms of added input delay at 60 ([240 fps post](https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5)).
- Microsoft's game streaming documents split the chain into capture, encode, network, decode and present, which is the split we measure ([latency measurement](https://learn.microsoft.com/en-us/gaming/gdk/docs/features/common/game-streaming/game-streaming-latency-measurement?view=gdk-2604)). The same documents describe compensating for latency by projecting inputs forward; we do not, since the game is not ours.
- Our budget in the spec adds the engine's copy and a possible frame of fence wait, which a desktop-capture product does not have.

## Input forwarding and its security

Steam and Parsec inject the guest's input into the host's operating system, which is what lets a guest use a mouse and keyboard, and what lets a malicious guest do anything the host account can. Steam lets the host choose controller only or controller plus keyboard and mouse; Parsec defaults to controller only ([Parsec permissions](https://support.parsec.app/hc/en-us/articles/32381747079572-Hosting-and-Permissions)). Share play goes further: the guest's input never leaves the game. The engine receives pad states through the pad layer it already has, from a fixed list, and the operating system is not involved. The cost is that a guest cannot use a mouse; the benefit is that there is no way to turn a guest's input into anything but a pad.

## Audio

Voice and game sound share one problem: echo. The party's voice engine has none ([ADR 0014](adr/0014-voice-is-an-engine-without-a-transport.md)). Game sound is stereo music-like content, so the Opus mode and bit rate differ from voice. Opus is [RFC 6716](https://www.rfc-editor.org/rfc/rfc6716).

## Not verified

- The default congestion controller of the QUIC stack under iroh, and whether it can be changed.
- Vulkan Video encode support in current drivers.
- Whether the key H.264 patents have expired in the countries we care about.
- The effect of the relay's rate limit on a 6 Mbit/s datagram stream.
