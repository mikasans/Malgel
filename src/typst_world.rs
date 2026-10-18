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
    text::{Font, FontBook, FontInfo},
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
                    let _span = crate::perf::Span::new("font_slot_first_load")
                        .detail(format!("{} face={index}", path.display()));
                    let data = std::fs::read(path).ok()?;
                    let font = Font::new(Bytes::new(data), *index);
                    drop(_span);
                    font
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
    let _span = crate::perf::Span::new("system_font_db_scan");
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let db = Arc::new(database);
    crate::perf::mark(&format!("system_font_db_faces={}", db.faces().count()));
    db
});

/// Family / path names that strongly suggest a CJK or emoji face.
///
/// Used only by tests that assert CJK faces were indexed with real coverage.
#[cfg(test)]
fn is_cjk_or_emoji_face(family: &str, path: &std::path::Path) -> bool {
    let family = family.to_ascii_lowercase();
    let path = path.to_string_lossy().to_ascii_lowercase();
    let hay = format!("{family} {path}");
    const NEEDLES: &[&str] = &[
        "cjk",
        "source han",
        "sourcehan",
        "noto sans sc",
        "noto sans tc",
        "noto sans jp",
        "noto sans kr",
        "noto sans hk",
        "noto serif sc",
        "noto serif tc",
        "noto serif jp",
        "noto serif kr",
        "noto serif hk",
        "noto sans mono cjk",
        "wenquanyi",
        "wqy-",
        "droid sans fallback",
        "pl ming",
        "uming",
        "ukait",
        "ar pl",
        "simsun",
        "simhei",
        "simkai",
        "microsoft yahei",
        "msyh",
        "pingfang",
        "hiragino",
        "songti",
        "heiti",
        "stheit",
        "stheiti",
        "apple sd gothic",
        "nanum",
        "malgun",
        "gulim",
        "batang",
        "emoji",
        "seguiemj",
        "noto color emoji",
        "apple color emoji",
        "twemoji",
    ];
    NEEDLES.iter().any(|n| hay.contains(n))
}

/// The bundled fonts first, then the system's, so documents keep their
/// designed look while scripts and symbols the bundled fonts lack (CJK,
/// emoji, …) fall back to installed fonts.
///
/// Every system face is indexed with Typst's own [`FontInfo`] (family trim,
/// flags, axes, real cmap coverage) so `#set text(font: …)` and glyph
/// fallback match `typst` / LaTeX-like selection — not a fabricated Latin-ish
/// stub that made arbitrary faces look wrong. Face bytes are read through
/// fontdb's memmap ([`fontdb::Database::with_face_data`]) once per file while
/// building the book; the heap keeps only `FontInfo` + path slots until Typst
/// selects a face (then [`FontSlot`] loads it).
pub static DOCUMENT_FONTS: LazyLock<FontSet> = LazyLock::new(|| {
    let _span = crate::perf::Span::new("document_fonts_index");
    let (mut book, mut slots) = bundled_fonts();
    // One memmap parse per font file — TTC collections expose many faces.
    let mut infos_by_path: HashMap<PathBuf, Vec<FontInfo>> = HashMap::new();
    let mut files_parsed = 0usize;
    for face in SYSTEM_FONT_DB.faces() {
        let path = match &face.source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path.clone(),
            _ => continue,
        };
        if !infos_by_path.contains_key(&path) {
            let infos = SYSTEM_FONT_DB
                .with_face_data(face.id, |data, _| FontInfo::iter(data).collect::<Vec<_>>())
                .unwrap_or_default();
            files_parsed += 1;
            infos_by_path.insert(path.clone(), infos);
        }
        let Some(infos) = infos_by_path.get(&path) else {
            continue;
        };
        let Some(info) = infos.get(face.index as usize).cloned() else {
            continue;
        };
        book.push(info);
        slots.push(FontSlot::System {
            path,
            index: face.index,
            font: OnceLock::new(),
        });
    }
    let set = FontSet {
        book: LazyHash::new(book),
        slots,
    };
    crate::perf::mark(&format!(
        "document_fonts_slots={} font_files_parsed={files_parsed}",
        set.slots.len()
    ));
    set
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
    fn system_font_index_uses_typst_fontinfo() {
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
            "DOCUMENT_FONTS index took {index:?}; expected memmap FontInfo index"
        );
    }

    #[test]
    fn document_fonts_use_real_typst_fontinfo() {
        let fonts = &*DOCUMENT_FONTS;
        let mut book_len = 0usize;
        while fonts.book.info(book_len).is_some() {
            book_len += 1;
        }
        assert!(book_len > 1, "empty font book");
        // Bundled New Computer Modern / Libertinus should expose MATH or SERIF
        // flags from real FontInfo — fabricated stubs never set those.
        let has_math_or_serif = (0..book_len).any(|ix| {
            fonts.book.info(ix).is_some_and(|info| {
                info.flags.contains(typst::text::FontFlags::MATH)
                    || info.flags.contains(typst::text::FontFlags::SERIF)
            })
        });
        assert!(
            has_math_or_serif,
            "expected real FontInfo flags (MATH/SERIF) from Typst-parsed faces"
        );
        // A Latin-named system face must not advertise the old fake Latin-ish
        // stub alone: real cmap coverage for 'A' is expected, and family names
        // come from Typst's trim (not raw fontdb strings with style suffixes).
        let latin_ok = (0..book_len).any(|ix| {
            fonts.book.info(ix).is_some_and(|info| {
                info.coverage.contains(u32::from(b'A'))
                    && !info.family.to_ascii_lowercase().contains("bold")
            })
        });
        assert!(
            latin_ok,
            "expected Typst FontInfo with real coverage for Latin"
        );
    }

    #[test]
    fn arbitrary_system_font_selects_like_typst() {
        // Pick a real system family Typst indexed (not a fabricated stub).
        let fonts = &*DOCUMENT_FONTS;
        let mut book_len = 0usize;
        while fonts.book.info(book_len).is_some() {
            book_len += 1;
        }
        let family = (0..book_len).find_map(|ix| {
            let info = fonts.book.info(ix)?;
            let name = info.family.as_str();
            let lower = name.to_ascii_lowercase();
            // Prefer a distinctive installed face over bundled Liber* / New CM.
            if lower.contains("dejavu sans")
                || lower.contains("liberation sans")
                || lower.contains("noto sans")
                || lower.contains("inter")
                || lower.contains("passion one")
            {
                Some(name.to_string())
            } else {
                None
            }
        });
        let Some(family) = family else {
            eprintln!("skip arbitrary_system_font_selects_like_typst: no distinctive system face");
            return;
        };
        let dir = std::env::temp_dir().join(format!("malgel-arbfont-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("arb.typ");
        let source = format!(
            "#set text(font: {family:?}, size: 14pt)\n\nHello from {{family}}.\n\nThe quick brown fox.\n"
        );
        std::fs::write(&path, &source).unwrap();
        let mut live = crate::typst_live::LiveCompiler::new(&path);
        let compiled = live.compile(&source, 1);
        assert!(
            compiled.pages.as_ref().is_some_and(|p| !p.is_empty()),
            "Typst failed to shape with font {family:?}: {:?}",
            compiled.diagnostics
        );
        let img = &compiled.pages.as_ref().unwrap()[0].image;
        let b = img.bytes();
        assert!(
            b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP",
            "live sheet should be WebP for font {family}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chinese_text_selects_a_real_cjk_face() {
        let fonts = &*DOCUMENT_FONTS;
        let mut book_len = 0usize;
        while fonts.book.info(book_len).is_some() {
            book_len += 1;
        }
        let has_cjk = (0..book_len).any(|ix| {
            fonts
                .book
                .info(ix)
                .is_some_and(|info| info.coverage.contains('中' as u32))
        });
        // CI without a CJK package (or a stripped container) has nothing to
        // index; skip rather than false-fail. When CJK-named faces exist we
        // require real cmap coverage — that is the regression this catches.
        let system_has_cjk_named = SYSTEM_FONT_DB.faces().any(|face| {
            let family = face.families.first().map(|(n, _)| n.as_str()).unwrap_or("");
            let path = match &face.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path.as_path(),
                _ => return false,
            };
            is_cjk_or_emoji_face(family, path)
                && !family.to_ascii_lowercase().contains("emoji")
                && !path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("emoji")
        });
        if !system_has_cjk_named {
            eprintln!("skip chinese_text_selects_a_real_cjk_face: no CJK-named system fonts");
            return;
        }
        assert!(
            has_cjk,
            "no DOCUMENT_FONTS entry covers U+4E2D (中); CJK faces mis-indexed"
        );
        let dir = std::env::temp_dir().join(format!("malgel-cjk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cjk.typ");
        let source = "= 中文

你好，世界。
";
        std::fs::write(&path, source).unwrap();
        let mut live = crate::typst_live::LiveCompiler::new(&path);
        let compiled = live.compile(&source, 1);
        assert!(
            compiled.pages.as_ref().is_some_and(|p| !p.is_empty()),
            "Chinese Typst preview failed: {:?}",
            compiled.diagnostics
        );
        let _ = std::fs::remove_dir_all(&dir);
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
