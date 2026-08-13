#!/usr/bin/env python3
"""Generate and verify Codex Nexus branding outputs.

Design inputs live under assets/branding. Tauri outputs and tray runtime files
are generated under src-tauri/icons and must never become design inputs.
"""

from __future__ import annotations

import subprocess
import io
import shutil
import struct
import sys
import tempfile
from pathlib import Path

from PIL import Image


ROOT = Path(__file__).resolve().parents[1]
APP_MASTER = ROOT / "assets/branding/app/app-icon-master.png"
APP_MACOS = ROOT / "assets/branding/app/app-icon-macos.png"
TRAY_MACOS = ROOT / "assets/branding/tray/tray-macos.png"
TRAY_WINDOWS = ROOT / "assets/branding/tray/tray-windows.png"
ICON_OUTPUT = ROOT / "src-tauri/icons"
UNUSED_GENERATED_OUTPUTS = [ICON_OUTPUT / "icon.png"]


def resize_rgba(source: Path, size: int) -> Image.Image:
    with Image.open(source) as image:
        return image.convert("RGBA").resize(
            (size, size), Image.Resampling.LANCZOS
        )


def clear_outer_alpha(image: Image.Image) -> Image.Image:
    image = image.copy().convert("RGBA")
    pixels = image.load()
    last_x = image.width - 1
    last_y = image.height - 1
    for x in range(image.width):
        for y in (0, last_y):
            r, g, b, _ = pixels[x, y]
            pixels[x, y] = (r, g, b, 0)
    for y in range(image.height):
        for x in (0, last_x):
            r, g, b, _ = pixels[x, y]
            pixels[x, y] = (r, g, b, 0)
    return image


def write_tray_outputs() -> None:
    macos_16 = clear_outer_alpha(resize_rgba(TRAY_MACOS, 16))
    macos_32 = clear_outer_alpha(resize_rgba(TRAY_MACOS, 32))
    windows_32 = clear_outer_alpha(resize_rgba(TRAY_WINDOWS, 32))

    macos_dir = ICON_OUTPUT / "tray/macos"
    windows_dir = ICON_OUTPUT / "tray/windows"
    macos_dir.mkdir(parents=True, exist_ok=True)
    windows_dir.mkdir(parents=True, exist_ok=True)

    macos_16.save(macos_dir / "tray.png")
    macos_32.save(macos_dir / "tray@2x.png")
    windows_32.save(windows_dir / "tray.png")

    write_ico(
        TRAY_WINDOWS,
        windows_dir / "tray.ico",
        [16, 20, 24, 32, 40, 48, 64, 256],
    )


def clear_generated_png_edges() -> None:
    for path in ICON_OUTPUT.rglob("*.png"):
        with Image.open(path) as image:
            clear_outer_alpha(image).save(path)


def remove_unused_generated_outputs() -> None:
    for path in UNUSED_GENERATED_OUTPUTS:
        if path.exists():
            path.unlink()


def write_ico(source: Path, output: Path, sizes: list[int]) -> None:
    """Write PNG-backed ICO entries with a transparent perimeter per size."""
    payloads = []
    for size in sizes:
        image = clear_outer_alpha(resize_rgba(source, size))
        payload = io.BytesIO()
        image.save(payload, format="PNG")
        payloads.append((size, payload.getvalue()))

    header_size = 6 + 16 * len(payloads)
    directory = [struct.pack("<HHH", 0, 1, len(payloads))]
    offset = header_size
    for size, payload in payloads:
        dimension = 0 if size == 256 else size
        directory.append(
            struct.pack(
                "<BBBBHHII",
                dimension,
                dimension,
                0,
                0,
                1,
                32,
                len(payload),
                offset,
            )
        )
        offset += len(payload)

    output.write_bytes(b"".join(directory) + b"".join(payload for _, payload in payloads))


def rebuild_icns() -> None:
    """Round-trip ICNS through a cleaned iconset when iconutil is available."""
    iconutil = shutil.which("iconutil")
    icns = ICON_OUTPUT / "icon.icns"
    if not iconutil or not icns.exists():
        return

    with tempfile.TemporaryDirectory(prefix="codex-iconset-") as temp_dir:
        iconset = Path(temp_dir) / "icon.iconset"
        subprocess.run(
            [iconutil, "-c", "iconset", "-o", str(iconset), str(icns)],
            check=True,
        )
        for path in iconset.glob("*.png"):
            with Image.open(path) as image:
                clear_outer_alpha(image).save(path)
        subprocess.run(
            [iconutil, "-c", "icns", "-o", str(icns), str(iconset)],
            check=True,
        )


def generate() -> None:
    missing = [
        path for path in [APP_MASTER, APP_MACOS, TRAY_MACOS, TRAY_WINDOWS]
        if not path.exists()
    ]
    if missing:
        raise SystemExit("Missing branding input(s): " + ", ".join(map(str, missing)))

    # Generate the platform resources from the correct design input. The
    # macOS ICNS gets the white Squircle, while Windows/Store assets keep the
    # transparent brand Master instead of inheriting the macOS card.
    with tempfile.TemporaryDirectory(prefix="codex-tauri-icons-", dir=ROOT) as temp_dir:
        macos_output = Path(temp_dir) / "macos"
        subprocess.run(
            [
                "pnpm",
                "exec",
                "tauri",
                "icon",
                str(APP_MACOS),
                "--output",
                str(macos_output),
                "--ios-color",
                "transparent",
            ],
            cwd=ROOT,
            check=True,
        )

        subprocess.run(
            [
                "pnpm",
                "exec",
                "tauri",
                "icon",
                str(APP_MASTER),
                "--output",
                str(ICON_OUTPUT),
                "--ios-color",
                "transparent",
            ],
            cwd=ROOT,
            check=True,
        )

        shutil.copy2(macos_output / "icon.icns", ICON_OUTPUT / "icon.icns")

    clear_generated_png_edges()
    remove_unused_generated_outputs()
    write_ico(
        APP_MASTER,
        ICON_OUTPUT / "icon.ico",
        [16, 24, 32, 48, 64, 256],
    )
    write_tray_outputs()
    verify()


def edge_alpha(image: Image.Image) -> list[int]:
    alpha = image.getchannel("A")
    edge = []
    for x in range(image.width):
        edge.extend((alpha.getpixel((x, 0)), alpha.getpixel((x, image.height - 1))))
    for y in range(image.height):
        edge.extend((alpha.getpixel((0, y)), alpha.getpixel((image.width - 1, y))))
    return edge


def verify_png(path: Path, max_edge_alpha: int = 0) -> None:
    with Image.open(path) as image:
        if image.mode != "RGBA":
            raise AssertionError(f"{path}: expected RGBA, got {image.mode}")
        edge = edge_alpha(image)
        if max(edge, default=0) > max_edge_alpha:
            raise AssertionError(
                f"{path}: outer pixels are not fully transparent "
                f"(max alpha {max(edge)})"
            )


def verify_ico(path: Path) -> None:
    with Image.open(path) as image:
        for size in image.ico.sizes():
            frame = image.ico.getimage(size).convert("RGBA")
            edge = edge_alpha(frame)
            if max(edge, default=0) != 0:
                raise AssertionError(
                    f"{path} {size}: ICO frame outer alpha is not zero "
                    f"(max alpha {max(edge)})"
                )


def verify_icns() -> None:
    iconutil = shutil.which("iconutil")
    icns = ICON_OUTPUT / "icon.icns"
    if not iconutil or not icns.exists():
        return

    with tempfile.TemporaryDirectory(prefix="codex-verify-iconset-") as temp_dir:
        iconset = Path(temp_dir) / "icon.iconset"
        subprocess.run(
            [iconutil, "-c", "iconset", "-o", str(iconset), str(icns)],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        for path in iconset.glob("*.png"):
            # iconutil can quantize an antialiased ICNS edge to alpha=1.
            verify_png(path, max_edge_alpha=1)


def verify() -> None:
    if (ROOT / "app-icon.png").exists():
        raise AssertionError("legacy root app-icon.png must not remain")
    for path in UNUSED_GENERATED_OUTPUTS:
        if path.exists():
            raise AssertionError(f"unused generated icon must not remain: {path}")

    with Image.open(APP_MASTER) as master:
        if master.size != (1024, 1024) or master.mode != "RGBA":
            raise AssertionError("app-icon-master.png must be 1024x1024 RGBA")
        bbox = master.getchannel("A").getbbox()
        if bbox is None:
            raise AssertionError("app-icon-master.png has no visible Logo")
        left, top, right, bottom = bbox
        margins = (left, top, 1024 - right, 1024 - bottom)
        if min(margins) < 64:
            raise AssertionError(f"app-icon-master.png safety margin is too small: {margins}")

    with Image.open(APP_MACOS) as macos_app:
        if macos_app.size != (1024, 1024) or macos_app.mode != "RGBA":
            raise AssertionError("app-icon-macos.png must be 1024x1024 RGBA")
        alpha = macos_app.getchannel("A")
        if alpha.getpixel((0, 0)) != 0:
            raise AssertionError("app-icon-macos.png must keep its outer canvas transparent")
        if alpha.getpixel((512, 512)) == 0:
            raise AssertionError("app-icon-macos.png must contain an opaque card at its center")

    with Image.open(TRAY_MACOS) as macos:
        if macos.mode != "RGBA" or macos.size != (64, 64):
            raise AssertionError("tray-macos.png must be 64x64 RGBA")
        if any((r, g, b) != (0, 0, 0) for r, g, b, a in macos.getdata() if a):
            raise AssertionError("tray-macos.png must be black/transparent Template artwork")

    for path in sorted((ROOT / "assets/branding").rglob("*.png")):
        verify_png(path)
    for path in sorted(ICON_OUTPUT.rglob("*.png")):
        verify_png(path)
    verify_ico(ICON_OUTPUT / "icon.ico")
    verify_ico(ICON_OUTPUT / "tray/windows/tray.ico")
    verify_icns()

    background = ICON_OUTPUT / "android/values/ic_launcher_background.xml"
    if background.exists() and "transparent" not in background.read_text(encoding="utf-8"):
        raise AssertionError(f"{background}: generated Android background must stay transparent")

    print("Branding verification passed: all PNG outer pixels have Alpha=0.")


def main() -> None:
    command = sys.argv[1] if len(sys.argv) > 1 else "verify"
    if command == "generate":
        generate()
    elif command == "verify":
        verify()
    else:
        raise SystemExit(f"Usage: {Path(sys.argv[0]).name} [generate|verify]")


if __name__ == "__main__":
    main()
