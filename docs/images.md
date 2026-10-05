# Image pipeline (`pi-image`)

Native decode/orient/resize/encode, replacing pi's photon WASM. Resizing
uses `fast_image_resize` (SIMD, row-streamed), so peak memory is independent
of the source size. It matches pi's
strategy: keep the original if within limits, otherwise fit to `maxWidth` /
`maxHeight`, then return the first encoding under `maxBytes` in pi's order (PNG,
then JPEG at the configured quality and 85/70/55/40), shrinking by 25% per
retry.

Comparison on a 6000x4000 PNG (native, via
`cargo run --release -p pi-image --example resize`):

| | pi (photon WASM) | pi-image (native) |
| --- | --- | --- |
| dimensions | 2000x1333 | 2000x1333 |
| format | image/jpeg | image/jpeg |
| base64 size | 2,405,452 B | 2,405,532 B |
| time | 1,485 ms | 336 ms |
| peak RSS | 493 MB | 207 MB |
