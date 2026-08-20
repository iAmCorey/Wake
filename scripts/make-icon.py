"""Create the Windows multi-resolution icon from the supplied Wake artwork."""

from pathlib import Path
import sys

from PIL import Image


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates" / "wake" / "assets" / "icon-source.png"
OUTPUT = ROOT / "crates" / "wake" / "assets" / "icon.ico"
SIZES = (16, 24, 32, 48, 64, 96, 128, 256)


def main() -> None:
    if not SOURCE.exists():
        raise SystemExit(f"icon source not found: {SOURCE}")

    with Image.open(SOURCE) as source:
        image = source.convert("RGBA")
        side = min(image.width, image.height)
        left = (image.width - side) // 2
        top = (image.height - side) // 2
        image = image.crop((left, top, left + side, top + side))
        image.save(OUTPUT, format="ICO", sizes=[(size, size) for size in SIZES])

    print(f"Created {OUTPUT}")


if __name__ == "__main__":
    sys.exit(main())
