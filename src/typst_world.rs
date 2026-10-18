//! A sealed Typst environment shared by math rendering and PDF export.
//!
//! The compiler sees one source file, the fonts bundled with Typst, and any
//! files the caller hands over in memory (images, icons). It cannot read the
//! file system or reach the network.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, OnceLock},
};

use typst::{
    Library, LibraryExt as _, World,
    diag::{FileError, FileResult},
    foundations::{Bytes, Datetime, Duration},
    syntax::{FileId, Source},
    text::{
        Coverage, Font, FontBook, FontFlags, FontInfo, FontStretch, FontStyle, FontVariant,
        FontWeight,
    },
    utils::LazyHash,
};
use typst_layout::PagedDocument;

/// The Typst standard library, shared by every world in this app.
pub static LIBRARY: LazyLock<LazyHash<Library>> =
    LazyLock::new(|| LazyHash::new(Library::default()));

/// A font the compiler can use, loaded on first use.
enum FontSlot {
    Bundled(Font),
    System {
        path: PathBuf,
        index: u32,
        font: OnceLock<Option<Font>>,
    },
}

impl FontSlot {
    fn get(&self) -> Option<Font> {
        match self {
            FontSlot::Bundled(font) => Some(font.clone()),
            FontSlot::System { path, index, font } => font
                .get_or_init(|| {
                    let data = std::fs::read(path).ok()?;
                    Font::new(Bytes::new(data), *index)
                })
                .clone(),
        }
    }
}

/// The fonts a world offers, with the book the compiler selects them from.
pub struct FontSet {
    /// The metadata the compiler matches a font request against.
    pub book: LazyHash<FontBook>,
    slots: Vec<FontSlot>,
}

impl FontSet {
    /// The font at `index` in the book, or `None` when the index is out of
    /// range. Reading a system font's data happens once, on first use.
    pub fn font(&self, index: usize) -> Option<Font> {
        self.slots.get(index)?.get()
    }
}

fn bundled_fonts() -> (FontBook, Vec<FontSlot>) {
    let fonts: Vec<Font> = typst_assets::fonts()
        .flat_map(|data| Font::iter(Bytes::new(data)))
        .collect();
    let book = FontBook::from_fonts(&fonts);
    (book, fonts.into_iter().map(FontSlot::Bundled).collect())
}

/// Only the fonts bundled with Typst: fast to set up, identical everywhere.
pub static BUNDLED_FONTS: LazyLock<FontSet> = LazyLock::new(|| {
    let (book, slots) = bundled_fonts();
    FontSet {
        book: LazyHash::new(book),
        slots,
    }
});

/// Shared system font index (paths + face metadata). Built once via fontdb's
/// memmap-backed scan; never copies font files into the heap. Typst and Mermaid
/// both reuse this so a second full disk scan never runs.
pub static SYSTEM_FONT_DB: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    Arc::new(database)
});

/// Coverage claim used when indexing system faces from fontdb metadata only.
/// Exact cmap coverage is unknown until the face is loaded; claiming the full
/// Unicode range keeps CJK / emoji fallback candidates visible. The real file
/// is still read only inside [`FontSlot`] on first use.
static SYSTEM_FACE_COVERAGE: LazyLock<Coverage> = LazyLock::new(|| {
    // Run-length: 0 outside, then every Unicode scalar value inside.
    serde_json::from_value(serde_json::json!([0, 0x11_0000]))
        .expect("full Unicode coverage encoding")
});

/// Build Typst [`FontInfo`] from fontdb face metadata — no `std::fs::read`.
fn font_info_from_face(face: &fontdb::FaceInfo) -> Option<FontInfo> {
    let family = face.families.first()?.0.clone();
    let style = match face.style {
        fontdb::Style::Normal => FontStyle::Normal,
        fontdb::Style::Italic => FontStyle::Italic,
        fontdb::Style::Oblique => FontStyle::Oblique,
    };
    let mut flags = FontFlags::empty();
    if face.monospaced {
        flags.insert(FontFlags::MONOSPACE);
    }
    Some(FontInfo {
        family,
        variant: FontVariant {
            style,
            weight: FontWeight::from(face.weight),
            stretch: FontStretch::from_number(face.stretch.to_number()),
        },
        flags,
        axes: Vec::new(),
        coverage: SYSTEM_FACE_COVERAGE.clone(),
    })
}

/// The bundled fonts first, then the system's, so documents keep their
/// designed look while scripts and symbols the bundled fonts lack (CJK,
/// emoji, …) fall back to installed fonts.
///
/// Indexing walks fontdb face metadata only (family / weight / style / path).
/// Font *bytes* are never `std::fs::read` into a `Vec` here — that happens
/// once inside [`FontSlot`] when Typst actually selects the face.
pub static DOCUMENT_FONTS: LazyLock<FontSet> = LazyLock::new(|| {
    let (mut book, mut slots) = bundled_fonts();
    for face in SYSTEM_FONT_DB.faces() {
        let path = match &face.source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path.clone(),
            _ => continue,
        };
        let Some(info) = font_info_from_face(face) else {
            continue;
        };
        book.push(info);
        slots.push(FontSlot::System {
            path,
            index: face.index,
            font: OnceLock::new(),
        });
    }
    FontSet {
        book: LazyHash::new(book),
        slots,
    }
});

/// Files available to the document, keyed by their path from the project
/// root without a leading slash (`image("/img-1.png")` → `img-1.png`).
pub type VirtualFiles = HashMap<String, Bytes>;

struct SandboxWorld {
    source: Source,
    files: VirtualFiles,
    fonts: &'static FontSet,
}

impl World for SandboxWorld {
    fn library(&self) -> &LazyHash<Library> {
        &LIBRARY
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.fonts.book
    }

    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let path = id.vpath().get_without_slash();
        self.files.get(path).cloned().ok_or(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.slots.get(index)?.get()
    }

    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        None
    }
}

/// Compile Typst `source` into pages. The error is the first diagnostic,
/// worded for the user.
pub fn compile(
    source: String,
    files: VirtualFiles,
    fonts: &'static FontSet,
) -> Result<PagedDocument, String> {
    let world = SandboxWorld {
        source: Source::detached(source),
        files,
        fonts,
    };
    let result = typst::compile::<PagedDocument>(&world).output;
    // Typst memoizes layout across compilations; keep only recent entries so
    // many distinct inputs don't accumulate.
    comemo::evict(30);
    result.map_err(|errors| {
        errors
            .first()
            .map(|error| error.message.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    })
}

/// Rasterize a laid-out page as PNG.
pub fn rasterize(page: &typst_layout::Page, pixels_per_point: f32) -> Result<Vec<u8>, String> {
    let pixmap = typst_render::render(
        page,
        &typst_render::RenderOptions {
            pixel_per_pt: (pixels_per_point as f64).into(),
            render_bleed: false,
        },
    );
    pixmap
        .encode_png()
        .map_err(|error| format!("Couldn’t encode the image: {error}."))
}

/// Rasterize an SVG image (for formats that only take bitmaps), returning
/// the PNG and the image's size in points. Text in the SVG may use any
/// installed font.
pub fn svg_to_png(svg: Vec<u8>, pixels_per_point: f32) -> Result<(Vec<u8>, f32, f32), String> {
    let mut files = VirtualFiles::new();
    files.insert("image.svg".into(), Bytes::new(svg));
    let document = compile(
        "#set page(width: auto, height: auto, margin: 0pt, fill: none)\n#image(\"/image.svg\")\n"
            .into(),
        files,
        &DOCUMENT_FONTS,
    )?;
    let page = document
        .pages()
        .first()
        .ok_or_else(|| "The image is empty.".to_string())?;
    let size = page.frame.size();
    Ok((
        rasterize(page, pixels_per_point)?,
        size.x.to_pt() as f32,
        size.y.to_pt() as f32,
    ))
}

/// Escape `text` for use inside a Typst string literal.
pub fn string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Escape `text` so Typst markup shows it literally.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '#' | '*' | '_' | '`' | '$' | '<' | '>' | '@' | '[' | ']' | '~' | '=' | '-'
            | '+' | '/' | '"' | '\'' => {
                out.push('\\');
                out.push(ch);
            }
            // Line starts are structural in markup; text never starts one.
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_virtual_files_only() {
        let mut files = VirtualFiles::new();
        files.insert(
            "dot.svg".into(),
            Bytes::new(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>"#
                    .to_vec(),
            ),
        );
        assert!(compile("#image(\"/dot.svg\")".into(), files, &BUNDLED_FONTS).is_ok());
        assert!(
            compile(
                "#image(\"/etc/passwd\")".into(),
                VirtualFiles::new(),
                &BUNDLED_FONTS
            )
            .is_err()
        );
    }

    #[test]
    fn system_font_index_avoids_eager_heap_reads() {
        use std::time::Instant;
        let t0 = Instant::now();
        let faces = SYSTEM_FONT_DB.faces().count();
        let scan = t0.elapsed();
        let t1 = Instant::now();
        let fonts = &*DOCUMENT_FONTS;
        let index = t1.elapsed();
        let mut book_len = 0usize;
        while fonts.book.info(book_len).is_some() {
            book_len += 1;
        }
        assert!(
            book_len > 1 && faces > 0,
            "expected system faces (book={book_len}, fontdb={faces})"
        );
        // Indexing must stay well under the multi-second / GB-heap stall of the old
        // std::fs::read-every-font loop (typically tens–hundreds of ms).
        eprintln!(
            "SYSTEM_FONT_DB scan={scan:?} faces={faces}, DOCUMENT_FONTS index={index:?} book={book_len}"
        );
        assert!(
            index.as_secs() < 5,
            "DOCUMENT_FONTS index took {index:?}; expected path/metadata-only"
        );
    }

    #[test]
    fn escapes_markup_and_strings() {
        let doc = compile(
            format!(
                "{} {}",
                escape_markup("*not bold* #x $y$ // no comment <l> @r [c] 'q' \"q\""),
                "#raw(".to_string() + &string_literal("a \"b\" \\ c") + ")"
            ),
            VirtualFiles::new(),
            &BUNDLED_FONTS,
        );
        assert!(doc.is_ok(), "{doc:?}");
    }
}
