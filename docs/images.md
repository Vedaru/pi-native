# Images (`pi-image`)

Native image decode, resize, and encode for `read` attachments. The agent reads
what an image *says*, so every image is scaled to a small, consistent target:
**1024 px on the long edge, 1 MiB encoded** (`PIPELETS_IMAGE_MAX_DIM` /
`PIPELETS_IMAGE_MAX_BYTES`).

- **Header first**: dimensions come from the header before any pixels are read.
- **JPEG** decodes with `libjpeg-turbo-rs`'s scaled IDCT (all 16 libjpeg-turbo
  factors), so a large JPEG is never decoded at full size.
- **PNG** decodes scanline by scanline and is box-downsampled on the fly.
- **Orientation** (EXIF) is applied before the target math, matching pi's
  `Math.round`.
- The reduced bitmap is resized with a box/area filter (`image`'s `thumbnail`,
  output-only allocation), then encoded PNG-first and JPEG second at the
  configured quality and 85/70/55/40, shrinking by 25% per retry — pi's order.

`PIPELETS_IMAGE_MAX_DIM=2000` restores pi's exact target. Peak memory is
14–40 MB for any input and returns to ~5 MB; [performance.md](performance.md)
has the measured figures and the comparison against pi's photon/WASM pipeline.
