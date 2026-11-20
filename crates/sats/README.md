> ⚠️ **Internal Crate** ⚠️
>
> This crate is intended for internal use only. It is **not** stable and may change without notice.

### Benchmarking BSATN

Run `cargo bench --profile release -p spacetimedb-sats --bench bsatn`. To compare
codec changes with the same fixtures, pass `-- --save-baseline before` on the
first run, then `-- --baseline before` after the change.

The `encode` and `decode` cases time encoding into a new `Vec<u8>` and decoding
from an existing slice into owned values. The mixed-row group also measures a
reused writer and a borrowed reader. Fixtures cover numeric arrays, batches of ordinary and
`Pod` rows, and mixed rows with short ASCII and multibyte strings up to 4 KiB.
Fixture construction and compression are outside the timed loop.

### Choosing a fast path

* For an owned packet, `bsatn::to_vec` preallocates when the value has a cheap
  size hint. Derived products and sums include their fields or active payload.
  Long lists of fixed-width elements need only a count; long lists of variable-width
  elements skip the hint. It does not run the serializer twice. For repeated writes to the same buffer,
  use `buf.clear(); bsatn::to_writer(&mut buf, &row)?;` to reuse its capacity.
* When decoded text need only live as long as the input, use `&str` instead of
  `String`: both validate UTF-8, but only `String` copies the text.
* For batches of padding-free `#[repr(C)]` numeric rows, derive `bytemuck::Pod`
  and `bytemuck::Zeroable` along with `Copy` and `Clone`, and add
  `#[sats(bsatn_pod)]` to the SATS derive. This selects a checked bulk copy on
  little-endian targets; `Pod` alone is not a guarantee of BSATN field order.
  Ordinary rows need no annotation. Both paths produce identical wire bytes.
  The decoder constructs ordinary rows from their individual little-endian
  fields. Only the bulk encoder for unannotated, layout-compatible rows still
  uses a raw byte view.
* Ordinary SDK products can read or write adjacent numeric fields in a single
  span without a `repr` attribute. The wire order is declaration order even if
  Rust has padded or reordered the fields. When encoding a slice or `Vec` of
  small, all-numeric derived rows, the batch writer reserves the full payload
  and fills each row's final slot directly. This also applies inside products
  and when appending to a reused buffer.
