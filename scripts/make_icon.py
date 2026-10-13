"""Draws the Strela icon (an arrow with motion streaks on a teal-to-blue tile) into crates/app/assets/.

Run: python scripts/make_icon.py
Outputs icon.ico (16..256 px, exe resource), icon.png (256 px), icon64.rgba (raw window icon) and
icon32.rgba (raw tray fallback). Needs Pillow. No third-party artwork is used.
"""

from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "crates" / "app" / "assets"
S = 1024  # drawn large, downscaled with antialiasing


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient_tile() -> Image.Image:
    c0, c1 = (45, 212, 191), (59, 130, 246)  # teal -> blue
    g = Image.new("RGBA", (S, S))
    px = g.load()
    for y in range(S):
        for x in range(S):
            px[x, y] = lerp(c0, c1, (x + y) / (2 * S - 2)) + (255,)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((40, 40, S - 40, S - 40), radius=230, fill=255)
    out = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    out.paste(g, (0, 0), mask)
    return out


def draw() -> Image.Image:
    img = gradient_tile()
    d = ImageDraw.Draw(img)
    white = (255, 255, 255, 255)
    soft = (255, 255, 255, 150)
    # arrow pointing right-up: shaft + head, drawn as one polygon rotated by building it axis-aligned first
    arrow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    a = ImageDraw.Draw(arrow)
    cy = S // 2
    a.polygon(
        [
            (250, cy - 70), (560, cy - 70), (560, cy - 190), (800, cy),
            (560, cy + 190), (560, cy + 70), (250, cy + 70),
        ],
        fill=white,
    )
    # motion streaks behind the arrow
    a.rounded_rectangle((150, cy - 190, 330, cy - 150), radius=20, fill=soft)
    a.rounded_rectangle((110, cy + 150, 290, cy + 190), radius=20, fill=soft)
    arrow = arrow.rotate(-18, resample=Image.BICUBIC, center=(S // 2, S // 2))
    img.alpha_composite(arrow)
    return img


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    big = draw()
    big.resize((256, 256), Image.LANCZOS).save(OUT / "icon.png")
    big.resize((256, 256), Image.LANCZOS).save(
        OUT / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
    )
    (OUT / "icon64.rgba").write_bytes(big.resize((64, 64), Image.LANCZOS).tobytes())
    (OUT / "icon32.rgba").write_bytes(big.resize((32, 32), Image.LANCZOS).tobytes())
    print("written to", OUT)


if __name__ == "__main__":
    main()
