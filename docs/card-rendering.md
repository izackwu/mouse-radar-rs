# Card rendering (`src/card.rs`)

This is the highest-tribal-knowledge area of the codebase. The pipeline is:

1. Build an SVG string from `SVG_TEMPLATE` via `fill_template`.
2. Parse with `usvg::Tree::from_str` using the shared `FONTS` fontdb.
3. Render to a `tiny_skia::Pixmap` with `resvg`.
4. Encode PNG.

## Fonts and fallback (`FONTS` LazyLock)

The order of operations in the fontdb setup is load-bearing. Don't reorder without understanding why each step exists:

1. **Bundled fonts first** (`include_bytes!` Inter Regular + Bold, plus `fonts/emoji-subset.ttf` for the activity-type emojis on the left edge of the card). These ship with the binary so the deployed bot is never font-less.
2. **Platform color-emoji font preload via `load_font_file`** (Apple Color Emoji on macOS, Noto Color Emoji on Linux) — *before* `load_system_fonts`. usvg's fallback iterates fonts in registration order, so this makes the color emoji font rank ahead of monochrome symbol fonts (Menlo, Apple Symbols, STIX) that also claim text-default codepoints like ☕ U+2615.
3. **`load_system_fonts`** for CJK + the rest of the fallback chain.
4. **Strip LastResort faces** (both `LastResort` and the dot-prefixed `.LastResort` macOS internal name). `/System/Library/Fonts/LastResort.otf` claims to support every Unicode codepoint by design — usvg picks it first and renders everything as `?` boxes if you leave it in.

## Input sanitization

`strip_emoji_presentation_selectors` removes U+FE0E (VS-15) and U+FE0F (VS-16) from `athlete_name` and `title` before they hit the template. usvg's font fallback drops base+VS sequences to the renderer's "no glyph" rectangle even when the chosen face has both codepoints in its cmap (verified: Apple Color Emoji has both U+2601 and U+FE0F). We strip *only* VS-15/VS-16 — the supplementary VS block (U+E0100..U+E01EF, CJK Ideographic Variation Sequences) is left alone because stripping it would render the wrong glyph variant for Japanese family names.

## Known usvg limitations (do not "fix" — verified unsolvable in this stack)

- **ZWJ-joined emoji never compose**: 🏃‍♀️ = 🏃 + ZWJ + ♀ + VS-16. usvg renders the base char only, drops the rest. Tracking issue: [linebender/resvg#861](https://github.com/linebender/resvg/issues/861).
- **Regional indicator flag pairs disappear in mixed text**: 🇰🇷 renders bare in isolation but vanishes when adjacent to any other text, even inside its own `<tspan>`. Tracking: [linebender/resvg#916](https://github.com/linebender/resvg/issues/916).
- **Linux-only CJK + emoji tofu**: when a CJK string is immediately followed by a non-ASCII-presentation emoji in the same text run on Linux/Noto (e.g. `"黑影儿📺"`), the whole run renders as tofu boxes. macOS doesn't trip this because of Apple Color Emoji's fallback ranking. Workaround: keep a space between CJK and emoji, or wrap separately. Test fixture 09 uses `"🏃 서울 마라톤 🏆"` because the spaces save it.

## Snapshot tests

`cargo test --lib generate_snapshots` writes deterministic PNGs to `card-snapshots/` (gitignored). 10 fixtures cover Latin, emoji, CJK, mixed scripts, and variation selectors. The test only asserts file size > 1KB — it's a smoke test, not a pixel-diff. Visual review is manual. Use `./scripts/gen-linux-snapshots.sh` to also produce `card-snapshots-linux/` against the prod font stack when changing font/rendering code.

## When to leave the usvg path

We've deliberately decided to stay on usvg + tiny-skia for now. The escape hatch when the limitations finally outweigh the deployment heft is **headless Chromium** (e.g. `chromiumoxide`) rendering HTML/CSS — handles every Unicode case correctly but adds ~200MB to the runtime image. The Rust-native middle option (`parley` + `swash` for text layout, tiny-skia for raster) was evaluated and rejected as too much surface area for the gain. Don't switch stacks without a specific user-facing failure that justifies the cost.
