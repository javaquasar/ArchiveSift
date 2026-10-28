"""Generate deterministic PNG fixtures for the random showcase demo page."""

from pathlib import Path
import random
import struct
import zlib


WIDTH = 480
HEIGHT = 300
SEEDS = (101, 202, 303, 404, 505, 606)
OUTPUT = Path(__file__).parent / "media" / "random-showcase-004"


def chunk(kind: bytes, payload: bytes) -> bytes:
    checksum = zlib.crc32(kind + payload) & 0xFFFFFFFF
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", checksum)


def pixels(seed: int) -> bytearray:
    rng = random.Random(seed)
    first = tuple(rng.randrange(35, 220) for _ in range(3))
    second = tuple(rng.randrange(35, 220) for _ in range(3))
    data = bytearray(WIDTH * HEIGHT * 3)

    for y in range(HEIGHT):
        for x in range(WIDTH):
            blend = (x + y) / (WIDTH + HEIGHT - 2)
            offset = (y * WIDTH + x) * 3
            for channel in range(3):
                value = int(first[channel] * (1 - blend) + second[channel] * blend)
                data[offset + channel] = max(0, min(255, value))

    for _ in range(22):
        left = rng.randrange(WIDTH)
        top = rng.randrange(HEIGHT)
        right = min(WIDTH, left + rng.randrange(18, 130))
        bottom = min(HEIGHT, top + rng.randrange(18, 100))
        color = tuple(rng.randrange(20, 245) for _ in range(3))
        for y in range(top, bottom):
            for x in range(left, right):
                offset = (y * WIDTH + x) * 3
                data[offset : offset + 3] = bytes(color)

    return data


def write_png(path: Path, seed: int) -> None:
    source = pixels(seed)
    rows = bytearray()
    stride = WIDTH * 3
    for y in range(HEIGHT):
        rows.append(0)
        rows.extend(source[y * stride : (y + 1) * stride])

    header = struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 2, 0, 0, 0)
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", header)
    png += chunk(b"IDAT", zlib.compress(bytes(rows), level=9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def main() -> None:
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for index, seed in enumerate(SEEDS, start=1):
        write_png(OUTPUT / f"random-{index:03}.png", seed)


if __name__ == "__main__":
    main()
