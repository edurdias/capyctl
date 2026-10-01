#!/usr/bin/env python3
"""Build CapyCTL's brand files from the owner's original artwork.

    python3 scripts/brand/build.py SOURCE_DIR

SOURCE_DIR holds the originals under these names (the owner keeps them;
they are not committed because they are large):

    primary-dark.png  primary-light.png   full logo with tagline
    mark-dark.png     mark-light.png      mascot, arc and terminal, no text
    horizontal-dark.png horizontal-light.png
    wordmark-dark.png wordmark-light.png
    icon.png          head and prompt on a dark rounded square (favicons)
    avatar.png        mascot in a dark circle
    app-icon.png      optional: mascot on a dark rounded square (else icon.png)
    social.png        optional: wide banner (else composed from horizontal-dark)

Dark artwork sits on the site's dark background and light artwork on its
light one; each image's background is flood-filled from its edges to the
exact page colour, so the images blend into the page.

Writes docs/brand/*.png (for the README and anyone who needs a logo) and the
site's files under site/public/. Needs Pillow. Re-run it when the originals
change; the outputs keep their names.
"""
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

# site/src/styles/tokens.css: --capyctl-bg in the dark and light themes.
DARK_BG = (0x1C, 0x19, 0x17)
LIGHT_BG = (0xFA, 0xFA, 0xF9)

ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / "docs" / "brand"
SITE = ROOT / "site" / "public"


def load(src: Path, name: str) -> Image.Image:
    im = Image.open(src / f"{name}.png").convert("RGB")
    target = LIGHT_BG if name.endswith("-light") else DARK_BG if name.endswith("-dark") else None
    return flatten(im, target) if target else im


def background_mask(im: Image.Image, tolerance: int = 28) -> Image.Image:
    """White where the pixel is background: within `tolerance` of the
    border's median colour and connected to an edge. The artwork's own dark
    or light details, enclosed by its outlines, are not background."""
    w, h = im.size
    border = [im.getpixel((x, 0)) for x in range(0, w, 7)] + [im.getpixel((x, h - 1)) for x in range(0, w, 7)]
    ref = tuple(sorted(c[i] for c in border)[len(border) // 2] for i in range(3))
    diff = ImageChops.difference(im, Image.new("RGB", im.size, ref)).convert("L")
    near = diff.point(lambda v: 255 if v <= tolerance else 0)
    for xy in [(x, 0) for x in range(0, w, 16)] + [(x, h - 1) for x in range(0, w, 16)] + \
              [(0, y) for y in range(0, h, 16)] + [(w - 1, y) for y in range(0, h, 16)]:
        if near.getpixel(xy) == 255:
            ImageDraw.floodfill(near, xy, 128)
    return near.point(lambda v: 255 if v == 128 else 0)


def flatten(im: Image.Image, target: tuple[int, int, int]) -> Image.Image:
    """Repaint the background (see `background_mask`) with `target`, the
    page colour, since the generator's backgrounds are close but not equal."""
    out = im.copy()
    out.paste(Image.new("RGB", im.size, target), mask=background_mask(im))
    return out


def optional(src: Path, name: str) -> Image.Image | None:
    return load(src, name) if (src / f"{name}.png").exists() else None


def banner(src: Path, w: int, h: int) -> Image.Image:
    """The link-preview image: social.png when given, else the dark
    horizontal logo centred on the dark background."""
    social = optional(src, "social")
    if social is not None:
        # Fit the whole banner (it is wider than the preview shapes) and fill
        # above and below with the dark page colour, rather than cropping it.
        social = flatten(social, DARK_BG)
        if social.width / social.height <= w / h:
            return cover(social, w, h)
        fitted = width(social, w)
        canvas = Image.new("RGB", (w, h), DARK_BG)
        canvas.paste(fitted, (0, (h - fitted.height) // 2))
        return canvas
    logo = width(load(src, "horizontal-dark"), round(w * 0.82))
    canvas = Image.new("RGB", (w, h), DARK_BG)
    canvas.paste(logo, ((w - logo.width) // 2, (h - logo.height) // 2))
    return canvas


def width(im: Image.Image, w: int) -> Image.Image:
    return im.resize((w, round(im.height * w / im.width)), Image.LANCZOS)


def cover(im: Image.Image, w: int, h: int) -> Image.Image:
    """Scale to fill w x h, then crop the centre."""
    scale = max(w / im.width, h / im.height)
    im = im.resize((round(im.width * scale), round(im.height * scale)), Image.LANCZOS)
    left, top = (im.width - w) // 2, (im.height - h) // 2
    return im.crop((left, top, left + w, top + h))


def shape_bbox(im: Image.Image, threshold: int = 40) -> tuple[int, int, int, int]:
    """The box around everything that differs from the corner colour."""
    bg = Image.new("RGB", im.size, im.getpixel((2, 2)))
    diff = ImageChops.difference(im, bg).convert("L").point(lambda v: 255 if v > threshold else 0)
    box = diff.getbbox()
    if box is None:
        raise SystemExit("no shape found against the corner colour")
    return box


def masked(im: Image.Image, shape: str, size: int) -> Image.Image:
    """Crop the rounded square or circle out of its baked-in background and
    return it with transparent corners, size x size."""
    left, top, right, bottom = shape_bbox(im)
    side = max(right - left, bottom - top)
    cx, cy = (left + right) // 2, (top + bottom) // 2
    crop = im.crop((cx - side // 2, cy - side // 2, cx - side // 2 + side, cy - side // 2 + side))
    big = 4 * size  # draw the mask large, then shrink it, for smooth edges
    mask = Image.new("L", (big, big), 0)
    draw = ImageDraw.Draw(mask)
    if shape == "circle":
        # Inset slightly: the artwork's circle edge is antialiased into the
        # baked-in background, which would show as a light ring.
        inset = round(big * 0.012)
        draw.ellipse((inset, inset, big - 1 - inset, big - 1 - inset), fill=255)
    else:
        draw.rounded_rectangle((0, 0, big - 1, big - 1), radius=round(big * 0.22), fill=255)
    out = crop.resize((size, size), Image.LANCZOS).convert("RGBA")
    out.putalpha(mask.resize((size, size), Image.LANCZOS))
    return out


def unmix(im: Image.Image, bg: tuple[int, int, int]) -> Image.Image:
    """Turn flat ink on a flat background into ink on transparency.

    Each pixel is taken as a blend of the background and one of the image's
    inks (the two colours farthest from the background in different
    directions: the dark or white text, and the accent); its alpha is how far
    along that blend it sits, so antialiased edges stay smooth."""
    px = list(im.get_flattened_data() if hasattr(im, "get_flattened_data") else im.getdata())
    dist = lambda c: sum((c[i] - bg[i]) ** 2 for i in range(3))
    first = max(px, key=dist)
    def apart(c):  # far from the background and unlike the first ink
        return dist(c) * (sum((c[i] - first[i]) ** 2 for i in range(3)) ** 0.5)
    second = max(px, key=apart)
    inks = [first, second]
    vecs = [tuple(k[i] - bg[i] for i in range(3)) for k in inks]
    norms = [sum(v * v for v in vec) or 1 for vec in vecs]
    out = []
    for c in px:
        d = tuple(c[i] - bg[i] for i in range(3))
        best = None
        for ink, vec, norm in zip(inks, vecs, norms):
            t = max(0.0, min(1.0, sum(d[i] * vec[i] for i in range(3)) / norm))
            residual = sum((d[i] - t * vec[i]) ** 2 for i in range(3))
            if best is None or residual < best[0]:
                best = (residual, ink, t)
        _, ink, t = best
        out.append((*ink, round(255 * t)))
    rgba = Image.new("RGBA", im.size)
    rgba.putdata(out)
    return rgba.crop(rgba.getchannel("A").point(lambda a: 255 if a > 8 else 0).getbbox())


def lockup(icon: Image.Image, wordmark: Image.Image, height: int) -> Image.Image:
    """Icon and wordmark side by side on transparency, `height` px tall."""
    icon = icon.resize((height, height), Image.LANCZOS)
    text_h = round(height * 0.62)
    text = wordmark.resize((round(wordmark.width * text_h / wordmark.height), text_h), Image.LANCZOS)
    gap = round(height * 0.28)
    out = Image.new("RGBA", (height + gap + text.width, height), (0, 0, 0, 0))
    out.alpha_composite(icon, (0, 0))
    out.alpha_composite(text, (height + gap, (height - text_h) // 2))
    return out


def save_png(im: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, "PNG", optimize=True)


def save_jpeg(im: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, "JPEG", quality=85, optimize=True, progressive=True)


def save_webp(im: Image.Image, path: Path, quality: int = 80) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, "WEBP", quality=quality, method=6)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    src = Path(sys.argv[1])

    # README and general-use logos.
    for variant in ("dark", "light"):
        save_png(width(load(src, f"primary-{variant}"), 720), DOCS / f"logo-{variant}.png")
        save_png(width(load(src, f"horizontal-{variant}"), 1200), DOCS / f"horizontal-{variant}.png")
        save_png(width(load(src, f"wordmark-{variant}"), 1200), DOCS / f"wordmark-{variant}.png")
        save_png(width(load(src, f"mark-{variant}"), 512), DOCS / f"mark-{variant}.png")
    icon = masked(load(src, "icon"), "rounded", 512)
    save_png(icon, DOCS / "icon.png")
    app_icon = optional(src, "app-icon")
    save_png(masked(app_icon, "rounded", 512) if app_icon else icon, DOCS / "app-icon.png")
    save_png(masked(load(src, "avatar"), "circle", 512), DOCS / "avatar.png")
    # GitHub's repository social preview (Settings, 1280 x 640).
    save_jpeg(banner(src, 1280, 640), DOCS / "social-preview.jpg")

    # Site: hero logos (one per theme), header mark, favicons, link preview.
    for variant in ("dark", "light"):
        save_webp(width(load(src, f"primary-{variant}"), 560), SITE / "brand" / f"logo-{variant}.webp", 62)
    # Header lockups: the logo's own lettering on transparency, so the docs and
    # landing headers match the logo whatever their background.
    for variant, bg in (("dark", DARK_BG), ("light", LIGHT_BG)):
        word = unmix(load(src, f"wordmark-{variant}"), bg)
        save_png(word, DOCS / f"wordmark-{variant}-transparent.png")
        save_webp(lockup(icon, word, 64), SITE / "brand" / f"header-{variant}.webp", 85)
    icon.resize((48, 48), Image.LANCZOS).save(SITE / "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)])
    save_png(icon.resize((32, 32), Image.LANCZOS), SITE / "favicon-32.png")
    save_png(icon.resize((180, 180), Image.LANCZOS), SITE / "apple-touch-icon.png")
    save_png(icon.resize((512, 512), Image.LANCZOS), SITE / "icon-512.png")
    save_jpeg(banner(src, 1200, 630), SITE / "og.jpg")


if __name__ == "__main__":
    main()
