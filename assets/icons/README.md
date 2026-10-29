# Application icon

- `archive-sift.svg`: editable source artwork, designed for small application icons.
- `archive-sift.png`: 256×256 RGBA icon embedded in every native egui window.
- `archive-sift.ico`: PNG frames at 16, 24, 32, 48, 64, 128 and 256 pixels,
  embedded in the Windows executable by `build.rs`.

The main window, image viewer, description and queue share this icon. No image
files need to be installed next to the executable.

To export new artwork, render the SVG to a square PNG of at least 256×256 pixels,
then run:

```powershell
cargo run --locked --example export_app_icon -- target/rendered-icon.png assets/icons
```

For a Linux desktop entry, install `assets/archive-sift.desktop` under the user's
applications directory and `archive-sift.svg` under
`icons/hicolor/scalable/apps/archive-sift.svg` in the user's XDG data directory.
The `archive-sift` executable must be on `PATH`. The desktop entry and all window
app IDs use `archive-sift`, allowing Wayland desktops to resolve the same icon.
