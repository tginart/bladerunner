# Bladeworks terminal logo

The angular B mark has a vertical brush spine, a small upper bowl, and a
larger lower bowl. These text assets were converted from the selected
Bladeworks paper glyph and approved as the Bladerunner terminal logo.

- `bladeworks-full.txt`: 20 columns by 17 rows, Unicode braille. Used by the
  fullscreen hero box and tall stacked welcome screens.
- `bladeworks-compact.txt`: 14 columns by 12 rows, Unicode braille. Used by
  smaller stacked welcome screens and the minimal-mode welcome card.
- `bladeworks-ascii.txt`: 28 columns by 24 rows, plain ASCII for manual reuse.
  This larger alternative is not loaded by the welcome renderer.

`src/views/welcome/logo.rs` owns both runtime assets. The minimal interface
uses that same renderer through `minimal_api`, so it needs no duplicate art.
Existing theme colors, shimmer animation, and legacy-console hiding still
apply.

Keep every row of a runtime asset the same character width, including blank
braille cells: the renderer centers each line separately. Trimming rows
individually would distort the mark.
