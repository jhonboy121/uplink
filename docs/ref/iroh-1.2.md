# iroh 1.2 — API reference (verified)

Source: `~/.cargo/registry/src/*/iroh-1.2.0/src/` (endpoint.rs, endpoint/{presets,connection}.rs), `iroh-base-1.2.0/src/key.rs`,
`noq-proto-1.3.0/src/crypto/rustls.rs`. ✅ = compiles in uplink-core (clippy -D warnings, host + Android).

## Cargo

```toml
iroh = { version = "1.2", default-features = false, features = ["tls-aws-lc-rs", "portmapper", "metrics", "unstable-net-report"] }
rustls = { version = "0.23.33", default-features = false, features = ["aws_lc_rs", "std"] }  # iroh's pin
noq-proto = { version = "1.3", default-features = false }   # only to downcast HandshakeData
```
- Features: default = `metrics, fast-apple-datapath, portmapper, tls-ring`. We use **aws-lc-rs** (post-quantum ML-KEM). aws-lc
  builds for `aarch64-linux-android` through our clang wrapper with **no system CMake** ✅ (~1.5 min check).
- QUIC is `noq` (iroh's quinn fork). Types are re-exported under `iroh::endpoint::*` (Connection, SendStream, RecvStream, VarInt,
  ConnectionError, ReadExactError, WriteError, ClosedStream, …), but **not** `HandshakeData`.

## Keys and ids ✅

- `iroh::{SecretKey, PublicKey, EndpointId}` (`EndpointId = PublicKey`).
- `SecretKey::generate()` (no rand dependency needed), `to_bytes() -> [u8; 32]`, `from_bytes(&[u8; 32])`, `public()`.
- `PublicKey`: `Display` = hex, `FromStr` accepts base32/hex, `fmt_short()` (short hex), serde support.

## Endpoint ✅

```rust
let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)   // n0 relays + pkarr/DNS address lookup
    .crypto_provider(provider)            // Arc<rustls::crypto::CryptoProvider>; overrides the preset's (last call wins)
    .secret_key(secret)
    .alpns(vec![ALPN.to_vec()])
    .bind().await?;                       // iroh::endpoint::BindError
endpoint.id(); endpoint.addr(); endpoint.online().await;   // online = home relay connected
let connection = endpoint.connect(peer_id, ALPN).await?;   // impl Into<EndpointAddr>; ConnectError
let incoming = endpoint.accept().await;                     // Option<Incoming>; None once closed
let connection = incoming.accept()?.await?;                 // Incoming::accept -> Result<Accepting, ConnectionError>;
                                                             // Accepting: Future<Output = Result<Connection, ConnectingError>>
endpoint.close().await;
```
- Presets: `Empty`, `Minimal` (crypto provider only), `N0`, `N0DisableRelay`. With the `tls-*` features the crypto provider
  is optional. Set it explicitly anyway: no global `CryptoProvider::install_default` is needed.

## Connection ✅

`open_bi()`/`accept_bi()` → `(SendStream, RecvStream)`, `open_uni`/`accept_uni`, `send_datagram`/`read_datagram`,
`close(VarInt, &[u8])`, `closed().await`, `remote_id()`, `rtt()`, `stats()`, `paths()`, `handshake_data()`.
Streams: `write_all`, `finish()`, `read_exact`, `read_to_end`. `VarInt::from_u32` is `const`.

## Post-quantum check ✅

```rust
use noq_proto::crypto::rustls::HandshakeData;
connection.handshake_data()?.downcast::<HandshakeData>().ok()?.negotiated_key_exchange_group  // Option<rustls::NamedGroup>
```
The provider uses `kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768, aws_lc_rs::kx_group::X25519]`. X25519 is only for
relay/HTTPS servers; uplink **refuses peer connections that aren't `X25519MLKEM768`** (`crypto::require_post_quantum`, close
code `CLOSE_NOT_POST_QUANTUM`).

## Relays at runtime ✅ (read from 1.2's source; used by `uplink_core::pilot`)

- **Home relay choice** (`net_report.rs`, `add_report_history_and_set_preferred_relay`): every sweep (20–26 s, not
  configurable) probes **every relay in the map**; the home relay is the lowest latency seen over the last 5 min, and
  it only moves when the new one is **under 2/3** of the current one's latency. A relay that stops answering drops out
  of the report, so failover inside the map is iroh's own.
- **Live map changes:** `Endpoint::insert_relay(RelayUrl, Arc<RelayConfig>)` / `remove_relay(&RelayUrl)` (async).
  Each one is a major update that starts a fresh net report at once. No rebind is needed.
- **There is no getter for the map** on `Endpoint` (`relay_map()` exists only on `RelayMode`), so whoever changes it
  keeps its own copy.
- `RelayConfig::from(RelayUrl)` assumes QUIC address discovery on the default port (7842).
- Reaching a peer goes through **their** home relay whether or not it is in our map. The relay actor only looks the URL
  up in the map for an auth token.
- **Per-relay latencies:** `Endpoint::net_report()` → `Watcher<Value = Option<NetReport>>`, behind the
  `unstable-net-report` feature; types in `iroh::unstable_net_report::{NetReport, Probe, RelayLatencies}`.
  `report.relay_latency.iter()` yields `(Probe, &RelayUrl, Duration)`, one per probe kind (HTTPS, QAD v4/v6), so take
  the min per URL. `report.global_v4` / `global_v6` are the public addresses (compare the IPs, not the ports, to
  detect a new network).
- `home_relay_status()` → `Vec<RelayStatus>` with `url()` and `is_connected()`. It passes briefly through "none
  connected" while the home relay moves, so wait a few seconds before calling that a loss.

## Logging with explicit dispatch

iroh spawns its own tokio tasks, so a scoped `Dispatch` must be installed on every runtime thread:
`Builder::on_thread_start(move || std::mem::forget(tracing::dispatcher::set_default(&dispatch)))` (`uplink_core::runtime::build`).

## Gotcha: `proot` + parallel binds ✅ measured

Binding several endpoints concurrently under proot intermittently fails with `Failed to bind sockets: Not supported (os error 95)`.
noq-udp sets `IP_PKTINFO` with a hard `?` (other options tolerate `EOPNOTSUPP`), and proot's ptrace-emulated `setsockopt` fails under
concurrency. Measured: 8/10 parallel test runs failed, 0/10 serial. Not a device issue. Development moved off proot, so
`just test`/`just coverage` run in parallel again; set `RUST_TEST_THREADS=1` if you are ever back under proot.
