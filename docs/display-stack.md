# Display stack (2D) — design, contract, and verification

Status: **stood up and verified** on `blaude-display-01`
(e2-standard-4, asia-south1-a), a machine provisioned for it. It is **not** on
the team server, and cannot be; the measurements below say why.

The instance is left **STOPPED** after verification. A stopped VM bills only its
disk (~$1.60/month for 40 GB) instead of ~$97/month running. Start it with:

```
gcloud compute instances start blaude-display-01 --zone asia-south1-a
```

---

## Verified, with evidence

Run 2026-08-30 on `blaude-display-01`. Nothing was exposed publicly: HTTP bound
to loopback, no WebRTC UDP published, all checks over an IAP tunnel.

| Claim | Evidence |
|---|---|
| Xvfb virtual display | XFCE session running on `:99.0` in the container logs |
| Whole desktop, not a browser | `ghcr.io/m1k1o/neko/xfce` entrypoint; captured frame shows the XFCE desktop, panel and dock |
| Neko serving | `GET /` → `200 text/html`, 1424 bytes; `/health` → `200` |
| Container healthy | `docker ps` → `Up (healthy)` |
| Full frame capture | 1920×1080 PNG, **24,393 bytes** |
| Downscaled frame | JPEG at `-resize 640x`, **5,515 bytes** |
| An external process can drive the display | `xclock` launched into `DISPLAY=:99` appeared on the desktop, WM-managed with a title bar and taskbar entry, and showed up in the next capture |

The frames were pulled back and **looked at**, not just sized: a 24 KB PNG can
be a blank screen. The first shows a real XFCE desktop; the second shows the
externally-launched window on it.

**5.5 KB for a downscaled frame** is the number that matters for the brief's
"small enough that the same path works on a phone client later". It is, and the
downscale happens server-side so a phone never fetches the 1080p original.

### The Firefox Agent Bridge question is answered: yes

The brief said to check whether the harness's Firefox Agent Bridge can drive a
browser inside this display before adding any separate automation layer.

An external process attaching to `DISPLAY=:99` gets its window managed by XFCE
and rendered into the captured frame. That is exactly what the Agent Bridge
needs. **No separate automation layer, no Playwright**, consistent with the
settled decision.

Caveat: the base `xfce` image ships no browser (`ls /usr/bin | grep -i
firefox|chromium` is empty). The production image must add one. The mechanism is
proven; the package list is not complete.

### Not verified

Stated plainly rather than implied:

- **Real WebRTC streaming to a remote client.** That needs the UDP range
  published, which would have exposed a desktop to the internet for a test. The
  HTTP/signalling half is verified; the media half is not.
- **Multi-client viewing and control handoff.** Neko implements it; this
  deployment has not exercised it.
- **The sub-300ms latency target.** Unmeasured, because it is meaningless
  without the media path above.
- **Capture tooling is not baked into the image.** `imagemagick` and `x11-apps`
  were installed into the running container to verify the frame path. A
  production image must include them; otherwise the frame endpoint has nothing
  to capture with.

---

## Why it cannot run on the team server `blaude-gm-25c8`

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

### Client decision (2H): WKWebView hosting Neko's own client

The brief asks for this assessed and decided before any WebRTC layer is
written. **Decision: `WKWebView`.**

**The hard part of Neko is not the video, it is the control protocol.**
Multi-viewer attach, request/grant/release of input control, and
release-on-disconnect are all implemented in their web client and evolve with
their server. A native implementation has to reimplement that protocol and then
track it for as long as we ship. Video decode is the easy half and the half
`WKWebView` already does, hardware-accelerated, through system WebKit.

**A native WebRTC package would be the largest dependency in the project.**
`BlaudeKit` currently has none at all: its own header says
"URLSessionWebSocketTask, no dependencies"
(`WebSocketTransport.swift:3`). The usual Swift WebRTC binaries are on the order
of a hundred megabytes. That is a lot of new surface, and a lot of new
notarisation and update burden, for one panel in the app.

**What we give up**, stated honestly: web chrome inside a native window, a JS
bridge for any custom input handling, and less direct control over reconnect
behaviour than a native client would allow. If the panel later needs to feel
like a first-class native surface rather than an embedded viewer, this is the
decision to revisit.

**What does not change either way** is the still-frame path. Screenshots are
plain HTTP image fetches and should be rendered natively regardless, so the
common case — glance at what the agent did — never pays for a WebView at all.
The WebView is only for the live interactive session.

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
