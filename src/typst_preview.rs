//! The preview a Typst document gets.
//!
//! Typst is not Markdown, so a `.typ` file is never read as text: opening one
//! compiles it with the same Typst compiler that exports PDFs and lays out
//! math, and the pages are shown in the PDF viewer. That makes a Typst
//! document's preview a live view of the compiler rather than a second,
//! divergent rendering of its source — and the same import, font and layout
//! rules the exported PDF follows.
//!
//! Two things distinguish this from a plain PDF file. The file it reads is
//! the `.typ` source, so the preview shows that file rather than a copy
//! somebody built; and the compiler is re-run whenever the source — or
//! anything it names — changes on disk, which is what keeps the preview
//! following an editor that is not this one: a `typst watch` workflow, a
//! terminal editor, another window on the same file.
//!
//! The compiler is given the document's folder and the folders the `typst`
//! command keeps packages in. A document names its own files by paths
//! relative to itself, so those are the files it reaches; a package it
//! imports is read out of the package folders, the same way the command
//! reads it, so a deck that imports `@preview/touying` previews the way it
//! would compile. The rest of the file system, the network and `sys.inputs`
//! stay out of reach, and a document depending on them fails with the
//! compiler's own message instead of showing a stale page. The files a
//! compilation read are recorded, and those are exactly the paths the
//! workspace watches.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use typst::{
    World,
    diag::{FileError, FileResult},
    foundations::{Bytes, Datetime, Duration},
    syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot, package::PackageSpec},
    text::Font,
};

#[cfg(test)]
use crate::pdf_view::PdfDocument;
use crate::{session::DiskStamp, typst_world};

/// The most bytes read from one file the document names. Sources and images
/// are small; the cap keeps a document pointing at something large from
/// pulling it all into memory.
const MAX_NAMED_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(test)]
/// What a Typst document compiles from and to: the source file, the other
/// files it named, and the drawn pages.
pub struct TypstPreview {
    /// The source file, which the preview re-reads on every refresh.
    source: PathBuf,
    /// Every file the last compilation read, the source first. These are the
    /// paths a change to which refreshes the preview.
    read: Vec<PathBuf>,
    /// The pages the source compiled to.
    pdf: PdfDocument,
}

#[cfg(test)]
impl TypstPreview {
    /// Compile the Typst source at `source` into pages.
    pub fn open(source: &Path) -> Result<Self, String> {
        let (read, pdf) = compile(source)?;
        Ok(Self {
            source: source.to_path_buf(),
            read,
            pdf,
        })
    }

    /// Every path a change to which refreshes the preview.
    pub fn watched(&self) -> &[PathBuf] {
        &self.read
    }

    /// The newest modification time and total size across the watched files,
    /// so one comparison notices a change to the document or any file it
    /// named — the same shape as the stamp one open file carries.
    pub fn stamp(&self) -> DiskStamp {
        stamp_of(&self.read)
    }

    /// Re-compile the source. A compile that fails keeps the pages already on
    /// screen and reports why, so saving a document mid-edit shows the last
    /// good version until it parses again.
    pub fn refresh(&mut self) -> Result<bool, String> {
        match compile(&self.source) {
            Ok((read, pdf)) => {
                self.read = read;
                self.pdf = pdf;
                Ok(true)
            }
            Err(err) => Err(err),
        }
    }

    /// The pages of the compiled document.
    pub fn document(&self) -> PdfDocument {
        self.pdf.clone_document()
    }
}

/// The newest modification time and total size of `paths`, so one stamp
/// covers a document and everything it imported.
pub fn stamp_of(paths: &[PathBuf]) -> DiskStamp {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut len = 0;
    for path in paths {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        let modified = metadata.modified().ok();
        newest = match (newest, modified) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
        len += metadata.len();
    }
    DiskStamp {
        modified: newest,
        len,
    }
}

#[cfg(test)]
/// Compile the source at `source`, returning the paths the compilation read
/// (the source first) and the resulting pages.
fn compile(source: &Path) -> Result<(Vec<PathBuf>, PdfDocument), String> {
    compile_in(source, package_folders())
}

#[cfg(test)]
/// Compile the source at `source` with `packages` as the folders a package
/// can come from, returning the paths the compilation read (the source
/// first) and the resulting pages.
fn compile_in(
    source: &Path,
    packages: Vec<PathBuf>,
) -> Result<(Vec<PathBuf>, PdfDocument), String> {
    let text = std::fs::read_to_string(source)
        .map_err(|err| format!("couldn’t read “{}”: {err}", source.display()))?;
    let mut read = vec![source.to_path_buf()];
    let world = DocumentWorld {
        root: source.parent().map(Path::to_path_buf).unwrap_or_default(),
        source: Source::new(file_id_for(source), text),
        read: Mutex::new(Vec::new()),
        packages,
        package_roots: Mutex::new(HashMap::new()),
    };
    let document = typst::compile(&world)
        .output
        .map_err(|errors| first_diagnostic(&errors))?;
    let bytes = typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default()).map_err(|errors| {
        let message = errors
            .first()
            .map(|error| error.message.to_string())
            .unwrap_or_default();
        format!("couldn’t write the pages: {message}")
    })?;
    let pdf =
        PdfDocument::from_bytes(bytes).map_err(|err| format!("couldn’t read the pages: {err}"))?;
    read.extend(world.read.into_inner().expect("the compile is done"));
    Ok((read, pdf))
}

#[cfg(test)]
/// The first diagnostic's message: the compiler's wording is already written
/// for the person who wrote the document.
fn first_diagnostic(errors: &[typst::diag::SourceDiagnostic]) -> String {
    errors
        .first()
        .map(|error| error.message.to_string())
        .unwrap_or_else(|| "the document could not be compiled".to_string())
}

/// A world that serves the document's own folder and the packages it imports.
///
/// The document's path decides its folder, and the paths the document names
/// are joined onto it, so nothing outside that folder is reachable — Typst
/// normalizes `..` away when it builds the path it asks for, so a document
/// cannot climb out of its own directory either. A path in a package is
/// joined onto that package's folder instead, which is where the `typst`
/// command reads it from too.
pub(crate) struct DocumentWorld {
    root: PathBuf,
    source: Source,
    /// The files this compilation read, for the watcher. `World`'s methods
    /// borrow, so the recording needs a lock rather than a field.
    read: Mutex<Vec<PathBuf>>,
    /// The folders a package can be found in, in the order they are tried.
    packages: Vec<PathBuf>,
    /// The folder each imported package was found in. `None` records a
    /// package that was looked for and is not there, so a document whose
    /// import is missing does not search the folders again for every path
    /// the failed import names.
    package_roots: Mutex<HashMap<PackageSpec, Option<PathBuf>>>,
}

impl DocumentWorld {
    /// A world for the document at `path` whose text is `text`, reading
    /// packages from the usual folders.
    pub(crate) fn new(path: &Path, text: String) -> Self {
        Self {
            root: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            source: Source::new(file_id_for(path), text),
            read: Mutex::new(Vec::new()),
            packages: package_folders(),
            package_roots: Mutex::new(HashMap::new()),
        }
    }

    /// Give the document new text. The source is edited rather than built
    /// anew, so the parser reuses what did not change, and the compiler's
    /// memoized work for unchanged parts stays valid.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.source.replace(text);
        self.read.get_mut().expect("not poisoned").clear();
    }

    /// The document's source as the compiler sees it.
    pub(crate) fn main_source(&self) -> &Source {
        &self.source
    }

    /// The files the last compile read besides the source.
    pub(crate) fn read_files(&self) -> Vec<PathBuf> {
        self.read.lock().expect("not poisoned").clone()
    }

    /// The folder a package's files are read from, looked up once per
    /// package.
    fn package_root(&self, spec: &PackageSpec) -> FileResult<PathBuf> {
        let mut found = self
            .package_roots
            .lock()
            .expect("a compile reads files on one thread");
        if let Some(folder) = found.get(spec) {
            return folder.clone().ok_or_else(|| missing_package(spec));
        }
        let folder = package_folder(&self.packages, spec);
        found.insert(spec.clone(), folder.clone());
        folder.ok_or_else(|| missing_package(spec))
    }

    /// The file `id` names, under the root it belongs to: the document's
    /// folder for the project, the package's folder for a package.
    fn path_of(&self, id: FileId) -> FileResult<PathBuf> {
        let folder = match id.root() {
            VirtualRoot::Project => self.root.clone(),
            VirtualRoot::Package(spec) => self.package_root(spec)?,
        };
        Ok(folder.join(id.vpath().get_without_slash()))
    }
}

/// The folders a package can come from, in the order the `typst` command
/// tries them: the data folder, where a person installs a package, then the
/// cache the command downloads into. `TYPST_PACKAGE_PATH` and
/// `TYPST_PACKAGE_CACHE_PATH` stand in for the two, the same variables the
/// command reads.
fn package_folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    match std::env::var_os("TYPST_PACKAGE_PATH") {
        Some(path) => folders.push(PathBuf::from(path)),
        None => folders.extend(dirs::data_dir().map(|dir| dir.join("typst/packages"))),
    }
    match std::env::var_os("TYPST_PACKAGE_CACHE_PATH") {
        Some(path) => folders.push(PathBuf::from(path)),
        None => folders.extend(dirs::cache_dir().map(|dir| dir.join("typst/packages"))),
    }
    folders
}

/// The folder a package's files live in: `namespace/name/version` under the
/// first folder that holds it, which is the layout the command writes.
fn package_folder(folders: &[PathBuf], spec: &PackageSpec) -> Option<PathBuf> {
    let relative = format!("{}/{}/{}", spec.namespace, spec.name, spec.version);
    folders
        .iter()
        .map(|folder| folder.join(&relative))
        .find(|folder| folder.is_dir())
}

/// A package the folders do not hold. Malgel compiles offline, so a package
/// has to be there already; saying which one and how to get it is more use
/// than a path next to the document that nothing will ever be at.
fn missing_package(spec: &PackageSpec) -> FileError {
    FileError::Other(Some(
        format!(
            "{spec} is not in the local Typst package folders — \
             compiling the document once with the Typst CLI downloads it"
        )
        .into(),
    ))
}

impl World for DocumentWorld {
    fn library(&self) -> &typst::utils::LazyHash<typst::Library> {
        &typst_world::LIBRARY
    }

    fn book(&self) -> &typst::utils::LazyHash<typst::text::FontBook> {
        // The document viewer's fonts: the bundled ones plus anything
        // installed, so CJK, emoji and a document's own families resolve.
        &typst_world::DOCUMENT_FONTS.book
    }

    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.source.id() {
            return Ok(self.source.clone());
        }
        let bytes = self.file(id)?;
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| FileError::InvalidUtf8)?;
        Ok(Source::new(id, text))
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let path = self.path_of(id)?;
        let metadata = std::fs::metadata(&path).map_err(|err| FileError::from_io(err, &path))?;
        if metadata.len() > MAX_NAMED_FILE_BYTES {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            return Err(FileError::Other(Some(
                format!("“{name}” is too large to be part of a document").into(),
            )));
        }
        let bytes = std::fs::read(&path).map_err(|err| FileError::from_io(err, &path))?;
        self.read
            .lock()
            .expect("a compile reads files on one thread")
            .push(path);
        Ok(Bytes::new(bytes))
    }

    fn font(&self, index: usize) -> Option<Font> {
        typst_world::DOCUMENT_FONTS.font(index)
    }

    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        None
    }
}

impl typst_ide::IdeWorld for DocumentWorld {
    fn upcast(&self) -> &dyn World {
        self
    }
}

/// The id the compiler identifies the document by.
///
/// The name is the file's own, so paths the document names resolve next to it
/// — which is where the files it names actually live. Ids are interned, so a
/// document compiled again reuses one entry rather than exhausting the
/// interner, and the id is only ever compared, never used to read from disk.
fn file_id_for(path: &Path) -> FileId {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document.typ".to_string());
    let vpath = VirtualPath::new(&name).unwrap_or_else(|_| {
        VirtualPath::new("document.typ").expect("a plain file name is a valid path")
    });
    FileId::new(RootedPath::new(VirtualRoot::Project, vpath))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("malgel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A document that names a sibling resolves it relative to itself, not to
    /// anything the caller picked.
    #[test]
    fn imports_resolve_next_to_the_document() {
        let dir = workspace("include");
        std::fs::write(dir.join("part.typ"), "= From a sibling\n").unwrap();
        let main = dir.join("main.typ");
        std::fs::write(&main, "#include \"part.typ\"\n").unwrap();

        let preview = TypstPreview::open(&main).expect("compile the include");
        // The sibling was read, so it is one of the paths watched: a change to
        // it refreshes the preview, which is the point of watching imports.
        assert_eq!(preview.document().page_count(), 1);
        assert!(
            preview
                .watched()
                .iter()
                .any(|path| path.ends_with("part.typ"))
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A missing import is a diagnostic naming the file, not a panic.
    #[test]
    fn a_missing_import_is_reported() {
        let dir = workspace("missing");
        let main = dir.join("main.typ");
        std::fs::write(&main, "#include \"nowhere.typ\"\n").unwrap();

        let Err(error) = TypstPreview::open(&main) else {
            panic!("the include is missing");
        };
        assert!(error.contains("nowhere.typ"), "got: {error}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A successful refresh recompiles; a failed one keeps the pages.
    #[test]
    fn refresh_keeps_the_pages_when_the_compile_fails() {
        let dir = workspace("refresh");
        let main = dir.join("main.typ");
        std::fs::write(&main, "= First\n").unwrap();
        let mut preview = TypstPreview::open(&main).unwrap();

        std::fs::write(&main, "= Changed\n\nMore text.\n").unwrap();
        assert!(preview.refresh().expect("recompiles"));
        assert_eq!(preview.document().page_count(), 1);

        // A broken document reports the error and leaves the good pages.
        std::fs::write(&main, "= Broken\n#oops(\n").unwrap();
        assert!(preview.refresh().is_err());
        assert_eq!(preview.document().page_count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An image beside the document is read and embedded, so a preview of a
    /// document with a figure looks like the exported PDF.
    #[test]
    fn reads_an_image_next_to_the_document() {
        let dir = workspace("image");
        std::fs::write(dir.join("dot.png"), tiny_png()).unwrap();
        let main = dir.join("main.typ");
        std::fs::write(&main, "#image(\"dot.png\")\n").unwrap();

        let preview = TypstPreview::open(&main).expect("compile the image");
        assert_eq!(preview.document().page_count(), 1);
        assert!(
            preview
                .watched()
                .iter()
                .any(|path| path.ends_with("dot.png"))
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file too large to be something a document named is refused.
    #[test]
    fn refuses_an_oversized_named_file() {
        let dir = workspace("large");
        let main = dir.join("main.typ");
        std::fs::write(&main, "#include \"big.typ\"\n").unwrap();
        // A hole rather than real bytes: the size is what the reader sees.
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("big.typ"))
            .unwrap()
            .set_len(MAX_NAMED_FILE_BYTES + 1)
            .unwrap();

        let Err(error) = TypstPreview::open(&main) else {
            panic!("the include is too large");
        };
        assert!(
            error.contains("big.typ") || error.contains("large"),
            "got: {error}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The stamp covers the document and everything it named: a change to any
    /// of them refreshes the preview, which is the whole point of watching
    /// the imports at all.
    #[test]
    fn the_stamp_covers_every_file_read() {
        let dir = workspace("stamp");
        std::fs::write(dir.join("part.typ"), "= Part\n").unwrap();
        let main = dir.join("main.typ");
        std::fs::write(&main, "#include \"part.typ\"\n").unwrap();

        let preview = TypstPreview::open(&main).unwrap();
        let before = preview.stamp();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("part.typ"), "= Part, revised\n").unwrap();
        assert_ne!(before, preview.stamp(), "a change to an import was missed");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A document that imports a package compiles to the package's own entry
    /// point: the package's files come from the package folders, the way the
    /// `typst` command reads them, and the folder the document sits in is not
    /// consulted for them.
    #[test]
    fn imports_a_package_from_the_package_folders() {
        let dir = workspace("package");
        let folder = dir.join("packages/preview/greeting/0.1.0");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("typst.toml"),
            "[package]\nname = \"greeting\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n",
        )
        .unwrap();
        std::fs::write(folder.join("lib.typ"), "#let greeting = [A package page]\n").unwrap();

        let main = dir.join("main.typ");
        std::fs::write(
            &main,
            "#import \"@preview/greeting:0.1.0\": greeting\n#greeting\n",
        )
        .unwrap();

        let (read, document) =
            compile_in(&main, vec![dir.join("packages")]).expect("compile the package import");
        assert_eq!(document.pages().len(), 1);
        assert!(
            read.iter().any(|path| path.ends_with("lib.typ")),
            "the package's entry point was not read: {read:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A package that is not installed is reported by its name, not by a path
    /// in the document's folder that nothing could ever be at — which is what
    /// a package import used to turn into.
    #[test]
    fn a_missing_package_names_the_package() {
        let dir = workspace("no-package");
        let main = dir.join("main.typ");
        std::fs::write(&main, "#import \"@preview/nowhere:1.2.3\"\n").unwrap();

        let Err(error) = compile_in(&main, Vec::new()) else {
            panic!("the package is not installed");
        };
        assert!(error.contains("@preview/nowhere:1.2.3"), "got: {error}");
        assert!(!error.contains("searched at"), "got: {error}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The folders are tried in order, so a package a person installed wins
    /// over a copy the command cached.
    #[test]
    fn the_first_package_folder_that_has_the_package_wins() {
        let dir = workspace("precedence");
        let spec: PackageSpec = "@preview/greeting:0.1.0".parse().unwrap();
        let data = dir.join("data");
        let cache = dir.join("cache");
        std::fs::create_dir_all(data.join("preview/greeting/0.1.0")).unwrap();
        std::fs::create_dir_all(cache.join("preview/greeting/0.1.0")).unwrap();

        let found = package_folder(&[data.clone(), cache], &spec).expect("the package is there");
        assert!(found.starts_with(&data), "got: {found:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A 1x1 transparent PNG, so a test needs no fixture file.
    fn tiny_png() -> Vec<u8> {
        [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
        .to_vec()
    }
}
