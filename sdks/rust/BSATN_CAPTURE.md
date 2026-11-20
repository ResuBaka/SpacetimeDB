# Opt-in server message capture

On native clients, configure a generated connection builder with:

```rust,ignore
let builder = DbConnection::builder()
    .with_bsatn_capture("/secure/unique-connection.capture", 512 * 1024 * 1024);
```

The limit is in bytes and includes metadata. Capture stops before a message
would exceed the limit; it does not skip large messages and continue. The path
must not exist, including on reconnect: provide a unique path per connection.
File creation failures fail the builder; later capture failures are logged and
disable capture without disconnecting. The setting is unavailable in browser.

**Captures contain secrets**, including the authentication token from the
initial connection, and potentially private application data. Do not commit or
share them. Unix files are created with permissions no broader than 0600;
protect their directory and delete the files when finished.

Capture incurs synchronous disk writes and an extra full decompression for each
message. It can affect throughput, latency and memory, particularly with the
streaming decoder. Only successfully decoded server messages are recorded.
The size limit applies to the file, not to transient decompression memory.

## Format version 1

All integers are little-endian. The file starts with the eight ASCII bytes
`STDBCAP1`. It then contains records in connection receive order:

| Bytes | Field |
| --- | --- |
| 8 | Unix timestamp in microseconds when captured (wall clock) |
| 8 | Original binary WebSocket payload length, including compression tag |
| 8 | Decompressed BSATN payload length |
| 1 | Original compression tag: 0 none, 1 Brotli, 2 Gzip |
| variable | Decompressed BSATN `ServerMessage`, without compression tag |

The timestamp is after decoding, not the exact network arrival time. The file
contains no WebSocket, TLS or TCP framing, no outgoing messages, and no protocol
version negotiation record. The current SDK uses v2 BSATN. A process crash or
disk error may leave a partial final record; readers must reject or ignore it.
Absence of a final record doesn't establish a complete capture.

## Replaying the BSATN codec

```sh
STDB_CAPTURE_PATH=/secure/connection.capture cargo run --release \
  -p spacetimedb-client-api-messages --example replay_bsatn
```

The offline replay rejects partial records and checks that every v2 message
consumes its entire payload and re-encodes byte-for-byte. It prints aggregate
encode/decode timings for the whole capture, excluding file I/O and compression.
Table rows inside a message are opaque bytes at this layer; measuring typed row
decoding requires the application's matching generated bindings.

### Region traffic measurement (2026-11-20)

The capture-guided codec pass used 124,644 messages and 1,777,208 typed row
occurrences across 50 tables. The baseline was the already optimized codec in
the working tree immediately before this pass. Both builds replayed the same
decompressed frames and generated region bindings with the client cache off.
Each entry is milliseconds for one full capture, averaging two process medians
of five warmed passes each (release, same CPU core, before/after/after/before).

| Operation | Before | After |
|---|---:|---:|
| Decode owned envelopes | 49.7 | 33.1 |
| Encode envelopes, fresh output | 29.8 | 17.1 |
| Encode envelopes, reused output | 11.0 | 10.3 |
| Decode individual typed rows | 80.9 | 40.3 |
| Encode individual typed rows, fresh output | 104.3 | 54.1 |
| Decode envelopes and parse cacheless SDK updates | 141.8 | 95.1 |

Both envelopes and typed rows were checked for full input consumption and
byte-identical re-encoding before timing. The SDK measurement includes building
and dropping updates, but not dispatching callbacks, network I/O, or compression.

### `fearless_simd` follow-up on the same capture

Compared the optimized codec above with two experimental changes enabled
together: dispatching the fixed-width array decode loop to the host SIMD level
for spans of at least 128 bytes and four elements, and copying length-prefixed
payloads of at least 128 bytes in 32-byte vectors on encode. Both built in
release mode with the same bindings. On a Ryzen 9 9950X3D (`fearless_simd` 1.2.0,
AVX-512), three same-core runs per build gave these medians of five-pass medians:

| Milliseconds per full capture | Current codec | SIMD experiment |
|---|---:|---:|
| Decode owned envelopes | 34.9 | 33.6 |
| Encode envelopes, fresh output | 17.1 | 17.5 |
| Decode individual typed rows | 43.5 | 45.9 |
| Decode envelopes and parse cacheless SDK updates | 94.6 | 96.8 |

One narrower use did win: encoding all 444,929 captured `LocationState` rows
into one output buffer per list (16 lists). Each wire row is 28 bytes. Packing
two rows together with SIMD lane compression took 0.358–0.375 ms versus
0.429–0.439 ms for the derived encoder writing to the same kind of buffer.
A plain Rust two-row pack took 0.508–0.543 ms. In the reverse direction,
SIMD expansion lost: 0.610–0.619 ms versus 0.250–0.260 ms for derived decode.
All results were checked against the captured bytes. Batch encoding is separate
from parsing incoming updates; no SIMD dependency was added to the codec.

### SIMD dispatch placement and generated table lookup

On this capture the generated client selected a table parser 341,180 times
from 329 schema names. A prototype grouped names by byte length and compared
four-byte fingerprints, then compared the entire name before accepting a hit.
The vector version compared eight fingerprints at a time. Three interleaved
process runs per version on the same core (median of five warmed passes per
process) gave these full **envelope + cacheless `DbUpdate`** results:

| Generated parser dispatch | Median ms | Range across runs, ms |
|---|---:|---:|
| Current generated string `match` | 90.5 | 89.6–91.2 |
| Fingerprint filter, SIMD dispatched per name | 90.3 | 86.5–90.4 |
| Fingerprint filter, SIMD dispatched once per message | 87.0 | 84.9–88.6 |
| Fingerprint filter, scalar | 84.3 | 83.1–86.7 |

These prototype timings did not establish a speedup attributable to dispatch.
A subsequent comparison kept the bindings and SDK identical except for table
selection in `DbUpdate::try_from`. The index was slower in all 16 paired
replays (5.2% median slowdown); both versions produced matching results for all
124,637 transactions. The generator therefore uses the direct string `match`.

Release assembly showed that the direct match already branches on name
length. The index added 12.7 fingerprint checks per lookup, on average, before
the full comparison. `player_action_state` accounted for 35% of lookups and
was 18th in its 25-name bucket. At 2.74 lookups per transaction, isolated
lookup timings accounted for about 5 ns of the observed 19 ns conversion gap.
The indexed version also called all 329 parser wrappers out of line, versus
218 call sites in the direct version. The cost of that inlining difference
was not measured separately.

The full replay verified all envelopes and typed rows byte-for-byte. Short ASCII string
checks and specializing the 16- and 28-byte fixed-row list loops did not show a
repeatable whole-parser gain over their respective controls.

To rerun **just the table-name lookup** on another machine, generate a
standalone benchmark using the matching Rust bindings. The preparation step
accepts the generated string match or its scalar table index and expects ASCII
identifier spellings. It does not read the capture.

```sh
python3 sdks/rust/benches/dispatch_simd/prepare.py \
    /path/to/module_bindings/mod.rs /path/to/new-bench-directory
STDB_CAPTURE_PATH=/path/to/region.capture cargo run --release \
    --manifest-path /path/to/new-bench-directory/Cargo.toml
```

The benchmark checks the generated match, scalar filter, and SIMD filter on
known names and misses before timing them on names in capture order. Dispatch
once per **sweep** measures the best case for batching; the integrated SDK
prototype could only move dispatch out to the **message** boundary. The
benchmark's `fearless_simd` dependency is isolated from the SDK and SATS.

### Implemented numeric batch encoder

The BSATN encoder writes batches of ordinary derived numeric rows directly
into reserved output slots; no annotation or wire-format change is required.

Paired release replays on CPU 6 (three processes per build) compared the
pre-change client with bindings regenerated from the matching region schema:

| Operation, ms per capture | Before | After |
|---|---:|---:|
| Encode 444,929 `LocationState` rows in 16 fresh BSATN arrays | 0.437 | 0.230 |
| Same batches appended after a header in a reused buffer | 0.472 | 0.241 |

Both rows time **batch encoding**, not individual row
allocations or the incoming parser. Each array's element count and payload
were checked against the captured rows before timing.
