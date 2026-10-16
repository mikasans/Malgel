# Preview UI responsiveness & Typst navigation — click-test notes

## Binary

```sh
cargo build --release
# binary:
./target/release/malgel
```

Absolute path after a release build on this box:

`/workspace/repos/Malgel/target/release/malgel`

## Launch (preview-only Typst)

```sh
RUST_LOG=malgel=info,malgel::typst_live=debug,malgel::pdf_view=debug \
  ./target/release/malgel /workspace/repos/Malgel/samples/nav-preview.typ
```

Then press **Ctrl+3** (or click the eye / Preview title-bar button) for preview-only layout.

## What to verify (parent desktop click tests)

1. **Title-bar stays clickable while preview is open / compiling**
   - Open `samples/nav-preview.typ`, switch to Preview (Ctrl+3).
   - While pages are filling in (or after editing the `.typ` so it recompiles), click the layout buttons (Editor / Split / Preview) and Toggle Side Bar.
   - Expect: buttons respond within a frame or two; stderr shows `ui: title-bar layout button clicked` / `ui: title-bar SetLayout` without multi-second gaps.
   - Root cause fixed: PDF page SVG conversion no longer runs on the GPUI UI thread; Typst live sheets are rasterized to PNG on the compiler thread.

2. **Preview click → source (mutual jump feedback)**
   - In Split or Preview layout, click text on a preview page.
   - Expect: layout becomes Split if it was Preview-only; editor scrolls to the line; line flashes briefly; toast “Preview click: line N”; caret focused.

3. **Go to Definition (first slice)**
   - In the editor, place the caret on `@fig` or `@sec` and press **F12** (or Command Palette → Go to Definition).
   - Expect: jump to the labelled element / heading, with the same flash + toast.

4. **Logging**
   - With `RUST_LOG` as above, look for `typst live: compile … ms`, `pdf: page N converted … (off UI thread)`, and `ui: title-bar …` lines.

## Optional automated path

```sh
cargo build --release
scripts/e2e/gui_smoke.sh
```

See `scripts/e2e/README.md` for dependencies (Xvfb, xdotool, ImageMagick, mesa Vulkan).
