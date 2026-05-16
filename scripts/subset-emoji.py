# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "fonttools",
# ]
# ///
"""Regenerate emoji-subset.ttf from macOS Apple Color Emoji font."""
from pathlib import Path

from fontTools.subset import Subsetter
from fontTools.ttLib import TTFont

FONT = "/System/Library/Fonts/Apple Color Emoji.ttc"
EMOJIS = ["🏃", "🚴", "🥾", "🚶", "🏁", "⏱", "🏅"]
OUTPUT = Path(__file__).parent.parent / "fonts" / "emoji-subset.ttf"

font = TTFont(FONT, fontNumber=0)
subsetter = Subsetter()
subsetter.populate(unicodes=[ord(c) for c in EMOJIS])
subsetter.subset(font)

# Restore name table (stripped by subsetter)
orig = TTFont(FONT, fontNumber=0)
font["name"] = orig["name"]
orig.close()

font.save(OUTPUT)
print(f"Done: {OUTPUT} ({len(font.getGlyphOrder())} glyphs)")
font.close()
