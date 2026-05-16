# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "fonttools",
# ]
# ///
"""Regenerate emoji-subset.ttf from macOS Apple Color Emoji font."""
import subprocess, sys
from pathlib import Path
from fontTools.ttLib import TTFont

FONT = "/System/Library/Fonts/Apple Color Emoji.ttc"
UNICODES = ["U+1F3C3", "U+1F6B4", "U+1F97E", "U+1F6B6", "U+1F3C1", "U+23F1", "U+1F3C5"]
OUTPUT = Path(__file__).parent.parent / "fonts" / "emoji-subset.ttf"

result = subprocess.run([
    "uvx", "fonttools", "subset", FONT,
    "--font-number=0",
    "--unicodes=" + ",".join(UNICODES),
    "--output-file=" + str(OUTPUT),
], capture_output=True, text=True)
if result.returncode != 0:
    print(result.stderr)
    sys.exit(1)

orig = TTFont(FONT, fontNumber=0)
subset = TTFont(OUTPUT)
subset["name"] = orig["name"]
subset.save(OUTPUT)
print(f"Done: {OUTPUT} ({len(subset.getGlyphOrder())} glyphs)")
