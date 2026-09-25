# The wire

What two uplink builds say to each other, and the rules for changing it. The code is
`crates/uplink-core/src/protocol.rs`; this is the registry that code must agree with.

**Encoding:** protobuf, written as derived Rust structs with `prost` (no `.proto`, no `protoc`,
no build script). Every message is **length-delimited**: a varint length, then the body. The
format is protobuf's spec, not prost's: any conforming implementation reads the same bytes.

**Connection:** QUIC over iroh, ALPN **`uplink/1`**, peer authenticated by its key, key exchange
`X25519MLKEM768` or refused (`crypto::require_post_quantum`).

## Streams

| Where | Carries |
|---|---|
| One bidirectional stream per call | `Signal`s, one after another |
| One unidirectional stream per video frame | a `StreamHeader`, then the frame's bytes to the end of the stream |
| One datagram per voice packet | a `DatagramHeader`, then the Opus packet |

The first `Signal` each way carries a `Hello`: the caller's `Offer`, and the callee's `Accept` (or
`Incompatible` in its place).

## Messages and tags

```
Signal (oneof kind)            1 Offer(Hello)  2 Accept(Hello)  3 Reject  4 Busy  5 Hangup
                               6 KeyframeRequest  7 Incompatible(Hello)  8 Media(MediaState)
                               9 VideoAsk(VideoAsk)  10 VideoAnswer(VideoAnswer)
Hello                          1 protocol: uint32  2 app: string
                               3 supports: repeated Capability  4 requires: repeated Capability
                               5 setup: Setup (offers only)
Setup                          1 call: uint64  2 voice: bool  3 resume: bool
MediaState                     1 mic_off: bool  2 camera_off: bool  3 held: bool
VideoAsk                       1 withdrawn: bool
VideoAnswer                    1 accepted: bool
StreamHeader (oneof kind)      1 Video(FrameHeader)
FrameHeader                    1 sequence: uint64  2 capture_micros: uint64  3 keyframe: bool
                               4 config: bool  5 turns: uint32
DatagramHeader (oneof kind)    1 Audio(AudioHeader)
AudioHeader                    1 sequence: uint64  2 capture_micros: uint64
```

Unit signals (`Reject`, `Busy`, …) carry an empty message, so each can grow fields later.

**Calls that survive a drop.** A lost connection (idle timeout or reset, never a close) starts a
30 s grace. The side with the lower key re-dials with an `Offer` whose setup names the same `call`
and sets `resume`; the other side's engine hands it to the call in progress, which answers `Accept`
and closes the old connection with `6`. A resume offer that names no call in progress is answered
`Busy` (another call is up) or `Hangup` (nothing is), and never rings. Media keeps its sequence
numbers across the new connection, and the first video frame on it is a keyframe.

**For now both phones are assumed to run the same build** (2026-09-24): voice calls, the video ask
and resuming are not gated by capabilities. Before a build with them meets an older one in the
field, they each get one, per rule 4.

## Capabilities

| Number | Name | Meaning |
|---|---|---|
| 0 | Unspecified | protobuf's zero; never sent |
| 1 | Video | H.264 frames on per-frame streams, and keyframe requests |
| 2 | Voice | Opus packets in datagrams |

A build lists what it **supports** and what it **requires**. If either side requires something the
other does not support, the call ends as `Incompatible` — before ringing on the callee's side,
before any media on the caller's — and both say which phone is behind, with both app versions.
Nothing is required yet; the first feature an older build would break goes into `REQUIRED`.

## Close codes

QUIC application error codes: `0` hang-up, `1` rejected, `2` busy, `3` protocol error,
`4` not post-quantum, `5` incompatible, `6` rejoined (the call carried on over a newer connection).

## Rules

1. **A tag is never reused or renumbered**, not even after its field is gone. Retire it: leave a
   comment in the struct and in the table above.
2. **New fields are additions.** An older build skips a tag it does not know; a newer build reads a
   field an older one did not send as its default. So a new field's default must mean "as before".
3. **New kinds of signal, stream or datagram are new `oneof` variants.** An older build reads an
   unknown variant as none: an unknown signal is ignored, an unknown stream is stopped, an unknown
   datagram is dropped. Never a protocol error.
4. **A feature an older build would mishandle** — not merely ignore — gets a capability. Send it
   only when the other side's `Hello` supports it; require it only when there is no way to call
   without it.
5. **The ALPN changes only for a break these rules cannot absorb.** Then both are accepted side by
   side for a while, so old and new builds still reach each other.
6. **Every change comes with a test** in `protocol.rs`'s own style: the old shape decodes the new
   bytes and the new shape decodes the old, or the unknown kind reads as unknown.

## Stored data

Anything kept in the database that has a shape of its own is protobuf too, under the same rules:
today the call-quality summary (`quality::Quality`, the `calls.quality` column).
