# Material Icon Theme (bundled)

Declarative VS Code–compatible file icon theme used by Malgel's Explorer,
tabs, Quick Open and Recent.

Source: [material-icon-theme](https://github.com/material-extensions/vscode-material-icon-theme) (MIT).
Generated from `generateManifest()` — `theme.json` follows the
[VS Code file icon theme](https://code.visualstudio.com/api/extension-guides/file-icon-theme) schema
(`iconDefinitions`, `fileExtensions`, `fileNames`, `folderNames`, …).

## Swap / add a package

1. Create `icon-themes/<name>/` with `theme.json` + `icons/*.svg`.
2. Point `DEFAULT_ICON_THEME` in `src/file_icon.rs` at that folder
   (or set the setting once settings grow a picker).
3. Rebuild. Lucide glyphs remain the fallback when an id has no SVG.

Do not edit mappings by hand in Rust — edit the theme JSON (or drop in
another VS Code icon package exported to this shape).
