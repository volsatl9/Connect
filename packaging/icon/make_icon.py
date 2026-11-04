#!/usr/bin/env python3
"""Draws the app image: an open ring on indigo glass, with a node at each end for the two PCs.

Writes app/assets/icon/connect.png (1024 px), connect-256.png (for the window) and connect.ico
(for Windows). Needs Pillow. Run from the repository root.
"""
from PIL import Image, ImageChops, ImageDraw, ImageFilter

SIZE = 1024
SCALE = 4
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


def rounded_mask(size, inset, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((inset, inset, size - inset, size - inset), radius, fill=255)
    return mask


def gradient(size, top, bottom):
    column = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / (size - 1)
        column.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return column.resize((size, size))


def draw():
    size = SIZE * SCALE
    inset = 40 * SCALE
    mask = rounded_mask(size, inset, 220 * SCALE)
    image = gradient(size, (136, 142, 255), (70, 52, 214)).convert("RGBA")
    image.putalpha(mask)

    # The sheen of the glass across the top.
    sheen = Image.new("L", (size, size), 0)
    ImageDraw.Draw(sheen).ellipse((-size // 4, -size * 3 // 5, size * 5 // 4, size * 2 // 5), fill=58)
    sheen = ImageChops.multiply(sheen.filter(ImageFilter.GaussianBlur(40 * SCALE)), mask)
    image = Image.composite(Image.new("RGBA", (size, size), (255, 255, 255, 255)), image, sheen)
    image.putalpha(mask)

    # The ring, with its shadow.
    centre, radius, width = size // 2, 262 * SCALE, 96 * SCALE
    box = (centre - radius, centre - radius, centre + radius, centre + radius)
    start, end = 42, 318
    node = 82 * SCALE

    def ring(colour, offset=0):
        layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        canvas = ImageDraw.Draw(layer)
        shifted = tuple(v + offset for v in box)
        canvas.arc(shifted, start, end, fill=colour, width=width)
        from math import cos, radians, sin

        for angle in (start, end):
            x = centre + (radius - width / 2) * cos(radians(angle))
            y = centre + offset + (radius - width / 2) * sin(radians(angle))
            canvas.ellipse((x - node, y - node, x + node, y + node), fill=colour)
        return layer

    shadow = ring((24, 16, 90, 150), 14 * SCALE).filter(ImageFilter.GaussianBlur(18 * SCALE))
    image.alpha_composite(shadow)
    image.alpha_composite(ring((255, 255, 255, 255)))
    image.putalpha(ImageChops.multiply(image.getchannel("A"), mask))
    return image.resize((SIZE, SIZE), Image.LANCZOS)


def main():
    image = draw()
    image.save("app/assets/icon/connect.png")
    image.resize((256, 256), Image.LANCZOS).save("app/assets/icon/connect-256.png")
    image.save("app/assets/icon/connect.ico", sizes=[(s, s) for s in ICO_SIZES])


if __name__ == "__main__":
    main()
