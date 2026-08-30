# Display stack (2D) — design, contract, and why it is not deployed yet

Status: **designed, not stood up.** The current team server cannot run it. The
measurements and the reasoning are below so the decision is checkable rather
than asserted.

---

## Why it is not running on `blaude-gm-25c8`

Measured on the live box, 2026-08-30:

| Resource | Measured | What Neko + Xvfb needs |
|---|---|---|
| vCPU | **2 shared** (e2-small: 0.5 baseline, bursts to 2) | 2 dedicated minimum for software encode |
| RAM | **1.9 GiB total, 1.6 GiB available** (2 GiB swap already added) | Xvfb + XFCE + browser + gstreamer alone exceed this |
| Free disk | **3.9 GiB** (9.7 G disk, 59% used) | Docker + the Neko image is 1.5-3 GiB |
| Docker | **not installed**, service inactive | required by the shipped image |
| GPU | **no `/dev/dri`** | hardware encode impossible; falls back to CPU |

The blocking one is `/dev/dri`. With no GPU there is no hardware encoder, so
every frame is encoded on the CPU. Software-encoding a desktop stream at a
usable frame rate needs roughly a full core sustained; this box has half a core
baseline and shares its burst with **the agent daemon that is the actual
product**. The stream would either miss the sub-300ms target badly or starve
the daemon, and RAM would be exhausted before either happened.

Installing it here would take a working team server and break it. So it is
designed and specified, not deployed.

### What it needs instead

- 4 vCPU dedicated, 8 GiB RAM, 40 GiB disk as a floor (`e2-standard-4` or
  `n2-standard-4`).
- For real quality, a GPU instance exposing `/dev/dri` (`n1-standard-4` plus a
  T4). With NVENC, encode stops competing with the agent for CPU at all.
- Either a second VM dedicated to display, or a bigger single box. A separate
  VM is the better shape: display load then cannot degrade agent turns, and it
  can be sized and stopped independently.

Nothing else in this document depends on which of those is chosen.

---

## Architecture

```
   Xvfb :99  ──>  window manager (XFCE)  ──>  applications
        │
        ├──> Neko (gstreamer capture) ──> WebRTC / DTLS-SRTP ──> viewers
        │                                  signalling over WSS
        └──> screenshot endpoint ──────> PNG/JPEG frames ──> agent + UI
```

**Xvfb, not x11vnc against a real display.** There is no physical display on a
cloud VM, and a virtual framebuffer is what the agent's own browser automation
already expects.

**Neko over the alternatives**, per the settled decision: it does WebRTC with
multi-viewer and control handoff built in, which is exactly the hard part.
x11vnc has no browser client story, Kasm is a heavier platform than needed, and
Hyperbeam and Metastream are hosted products.

**Swap Neko's entrypoint.** The stock image launches Chrome as the only
application, which streams *a browser*. We want the whole X display, so the
entrypoint runs a window manager instead — their docs cover XFCE. That is what
makes an Xcode-less Linux desktop, a terminal, and a browser all visible in one
stream.

---

## The contract blaude-native builds against

This is the part 2D owes the client, and it is stable regardless of where the
stack ends up running.

### Discovery

The harness advertises display availability on the existing websocket. No new
transport, no polling.

```
ApiEvent::DisplayAvailable {
  session_id: String,
  /// WebRTC signalling endpoint, wss://, already authenticated by the
  /// session's bearer. Absent when this runtime has no display.
  stream_url: Option<String>,
  /// Cheap still-frame endpoint, always present when a display exists.
  frame_url: String,
  width: u32,
  height: u32,
}
```

A client that only wants stills ignores `stream_url` entirely. That is
deliberate: the phone client and the agent's own feedback loop should never
have to negotiate WebRTC to look at a screenshot.

### Still frames

```
GET  {frame_url}?w=<px>&fmt=<png|jpeg>&q=<1-100>
  -> 200 image/png | image/jpeg
  -> 204 when the display exists but has not painted yet
```

Server-side downscaling with `w` is required, not optional. The agent's visual
feedback loop and a phone both want a small image, and making them fetch a
1080p PNG to shrink it locally wastes the one resource this design is short of.

Frames are the same path for the agent and the UI. One implementation, so a
screenshot the agent reasons about is definitionally the one a person sees.

### Interactive stream

Standard WebRTC: offer/answer plus ICE over the signalling websocket, media
over DTLS-SRTP. Neko's own protocol carries control handoff, so the client does
not invent one:

- multiple viewers attach read-only by default
- exactly one holds input control at a time
- control is requested, granted, and released explicitly, and released
  automatically on disconnect

### Client decision, deferred to 2H

Native WebRTC via a Swift package versus a `WKWebView` hosting Neko's own web
client is a 2H question and is not prejudged here. The contract above is the
same either way, which is the point of specifying it in terms of URLs and
events rather than in terms of a client library.

---

## Firefox Agent Bridge

**Check this before adding any separate automation layer**, per the brief. The
harness ships a Firefox Agent Bridge, and if it can drive a browser inside this
X display, then agent browser automation and human-visible browsing are the
same session rather than two parallel stacks.

I have **not** verified this, because there is no display to test against. It
is the first thing to check once the stack is running: launch the bridge with
`DISPLAY=:99` and confirm its browser appears in the stream. If it does, no
Playwright, no second automation path, consistent with the settled decision.

---

## Open questions

1. Separate display VM or one bigger box? Separate is safer for agent latency
   and costs more.
2. GPU or CPU encode? CPU is viable on 4 dedicated vCPU at modest resolution;
   a T4 makes it a non-issue and roughly doubles the instance cost.
3. Does the display live per team or per member? Per member matches the
   per-Linux-user isolation now in place, and multiplies the cost by the team
   size. Per team is cheaper and means teammates share a desktop, which may
   actually be the desired product behaviour for pairing.
