//! Preview extensions beyond CommonMark and GFM: GitHub alerts, math and
//! Mermaid diagrams.

use crate::lru::SizedLru;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use gpui_kit::assets::IconName;
use gpui_kit::{
    App, FontWeight, Hsla, Image, ImageFormat, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, Rgba, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, WeakEntity, Window,
    base::{TextView, TextViewState},
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::Button,
        clipboard::Clipboard,
        h_flex,
        tab::{Tab, TabBar},
        text::{
            InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode,
            MarkdownParseContext, MarkdownPlugin, markdown_ast::Node,
        },
        v_flex,
    },
    div, img,
    prelude::FluentBuilder as _,
    px,
};

use crate::{
    actions::ViewDiagram,
    analysis::content_hash,
    diagram::{self, DiagramTheme},
    math::{self, MathStyle},
};

/// The parser configuration and plugins the preview renders with.
pub fn markdown_extensions(preview: &gpui_kit::Entity<TextViewState>) -> MarkdownExtensions {
    let cache = MathCache::new(preview);
    let diagrams = DiagramCache::new(preview);
    let tabs_state = TabsState::new(preview);
    let with_math = |extensions: MarkdownExtensions| {
        extensions
            .plugin(BlockMathPlugin {
                cache: cache.clone(),
            })
            .plugin(InlineMathPlugin {
                cache: cache.clone(),
            })
            .plugin(MermaidPlugin {
                cache: diagrams.clone(),
            })
    };
    // Alert bodies are rendered by their own nested view, which needs math too.
    let alert = AlertPlugin {
        body_extensions: with_math(MarkdownExtensions::default()),
    };
    let tabs = TabsPlugin {
        content_extensions: with_math(MarkdownExtensions::default().plugin(alert.clone())),
        state: tabs_state,
    };
    with_math(
        MarkdownExtensions::default()
            .frontmatter()
            .plugin(gpui_kit::component::text::FrontmatterPlugin::new())
            .plugin(alert)
            .plugin(tabs),
    )
}

// -------------------------------------------------------------------------
// GitHub alerts

/// The five GitHub alert types, e.g. `> [!NOTE]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl AlertKind {
    fn from_marker(marker: &str) -> Option<Self> {
        let name = marker.strip_prefix("[!")?.strip_suffix(']')?;
        Some(match name.to_ascii_uppercase().as_str() {
            "NOTE" => Self::Note,
            "TIP" => Self::Tip,
            "IMPORTANT" => Self::Important,
            "WARNING" => Self::Warning,
            "CAUTION" => Self::Caution,
            _ => return None,
        })
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Tip => "Tip",
            Self::Important => "Important",
            Self::Warning => "Warning",
            Self::Caution => "Caution",
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Note => IconName::Info,
            Self::Tip => IconName::Lightbulb,
            Self::Important => IconName::MessageSquareWarning,
            Self::Warning => IconName::TriangleAlert,
            Self::Caution => IconName::OctagonAlert,
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Self::Note => theme.info,
            Self::Tip => theme.success,
            Self::Important => theme.magenta,
            Self::Warning => theme.warning,
            Self::Caution => theme.danger,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Alert {
    kind: AlertKind,
    /// The alert's Markdown content, without the quote markers.
    body: SharedString,
}

/// Split a blockquote's source into an alert, if its first line is an alert
/// marker and nothing else, as GitHub requires.
pub fn parse_alert(blockquote_source: &str) -> Option<(AlertKind, String)> {
    let mut lines = blockquote_source.lines().map(|line| {
        let line = line.trim_start();
        match line.strip_prefix('>') {
            Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
            // A lazy continuation line.
            None => line,
        }
    });
    let kind = AlertKind::from_marker(lines.next()?.trim())?;
    let body = lines.collect::<Vec<_>>().join("\n");
    Some((kind, body.trim().to_string()))
}

#[derive(Clone)]
struct AlertPlugin {
    body_extensions: MarkdownExtensions,
}

impl MarkdownPlugin for AlertPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "github-alert"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Blockquote(_) = node else {
            return None;
        };
        let source = cx.node_source(node)?;
        let (kind, body) = parse_alert(source)?;
        Some(
            MarkdownNode::new(
                "github-alert",
                Alert {
                    kind,
                    body: body.clone().into(),
                },
            )
            .text(format!("{}\n{body}", kind.title()))
            .markdown(source),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let alert = node.data::<Alert>().expect("alert node data");
        let color = alert.kind.color(cx);
        let id = SharedString::from(format!("alert-{}", content_hash(&alert.body)));

        v_flex()
            .w_full()
            .gap_1()
            .px_4()
            .py_3()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(color.opacity(0.35))
            .bg(color.opacity(0.06))
            .child(
                h_flex()
                    .gap_2()
                    .text_color(color)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(Icon::new(alert.kind.icon()).small())
                    .child(alert.kind.title()),
            )
            .when(!alert.body.is_empty(), |this| {
                this.child(
                    TextView::markdown(id, alert.body.clone())
                        .markdown_extensions(self.body_extensions.clone())
                        .on_link_click(crate::links::on_click)
                        .w_full(),
                )
            })
    }
}

// -------------------------------------------------------------------------
// Multi-tab content blocks

/// A single tab item within a multi-tab block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabItem {
    pub title: SharedString,
    pub content: SharedString,
}

/// Parsed tabs block data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabsData {
    pub tabs: Vec<TabItem>,
    pub default_index: usize,
    /// Byte range of this block in the preview source (`offset`, `len`) —
    /// avoids retaining a second full copy beside [`MarkdownNode::markdown`].
    #[allow(dead_code)]
    pub source_range: (usize, usize),
}

/// Tracks the selected tab for each tab block in the preview.
#[derive(Clone)]
pub struct TabsState {
    active_tabs: Arc<Mutex<HashMap<u64, usize>>>,
    preview: WeakEntity<TextViewState>,
}

impl TabsState {
    pub fn new(preview: &gpui_kit::Entity<TextViewState>) -> Self {
        Self {
            active_tabs: Arc::default(),
            preview: preview.downgrade(),
        }
    }

    pub fn get_active(&self, block_id: u64, default_index: usize, count: usize) -> usize {
        let tabs = self.active_tabs.lock().expect("tabs state poisoned");
        match tabs.get(&block_id) {
            Some(&idx) if idx < count => idx,
            _ => default_index.min(count.saturating_sub(1)),
        }
    }

    pub fn set_active(&self, block_id: u64, index: usize, cx: &mut App) {
        self.active_tabs
            .lock()
            .expect("tabs state poisoned")
            .insert(block_id, index);
        _ = self.preview.update(cx, |preview, cx| {
            preview.invalidate_inline_layout(cx);
        });
    }
}

/// Parse a tab header delimiter line.
///
/// Returns `Some((title, is_default))` if the line is a valid tab header.
/// Supported markers:
/// - `=== "Title"` or `=== Title` (with optional `!` for default tab: `===! "Title"`)
/// - `== "Title"` or `== Title` (with optional `!` for default tab: `==! "Title"`)
/// - `@tab "Title"` or `@tab Title` (with optional `!` for default tab: `@tab! "Title"`)
/// - `:::tab "Title"` or `:::tab Title` (with optional `!` for default tab: `:::tab! "Title"`)
/// - `--- "Title"` or `--- Tab: Title`
pub fn parse_tab_header(line: &str) -> Option<(&str, bool)> {
    let trimmed = line.trim();
    let (rest, default) = if let Some(after) = trimmed.strip_prefix("===") {
        if let Some(after_bang) = after.strip_prefix('!') {
            (after_bang, true)
        } else {
            (after, false)
        }
    } else if let Some(after) = trimmed.strip_prefix("==") {
        if let Some(after_bang) = after.strip_prefix('!') {
            (after_bang, true)
        } else {
            (after, false)
        }
    } else if let Some(after) = trimmed.strip_prefix("@tab") {
        if let Some(after_bang) = after.strip_prefix('!') {
            (after_bang, true)
        } else {
            (after, false)
        }
    } else if let Some(after) = trimmed.strip_prefix(":::tab") {
        if let Some(after_bang) = after.strip_prefix('!') {
            (after_bang, true)
        } else {
            (after, false)
        }
    } else {
        let after = trimmed.strip_prefix("---")?;
        let after = after.trim_start();
        if after.is_empty() {
            return None;
        }
        let after = after
            .strip_prefix("tab:")
            .or_else(|| after.strip_prefix("Tab:"))
            .unwrap_or(after);
        (after, false)
    };

    let title = rest.trim();
    if title.is_empty() {
        return None;
    }

    // Strip surrounding quotes if present: "Title" or 'Title'
    let title = if (title.starts_with('"') && title.ends_with('"') && title.len() >= 2)
        || (title.starts_with('\'') && title.ends_with('\'') && title.len() >= 2)
    {
        &title[1..title.len() - 1]
    } else {
        title
    };

    let title = title.trim();
    if title.is_empty() {
        return None;
    }

    Some((title, default))
}

/// Strip common indentation across lines (e.g. 4-space indent in MkDocs tab content).
fn dedent_lines(lines: &[&str]) -> String {
    let min_indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);

    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        if line.trim().is_empty() {
            result.push("");
        } else if min_indent > 0 {
            let mut byte_idx = line.len();
            for (char_count, (idx, _)) in line.char_indices().enumerate() {
                if char_count == min_indent {
                    byte_idx = idx;
                    break;
                }
            }
            result.push(&line[byte_idx..]);
        } else {
            result.push(*line);
        }
    }

    result.join("\n").trim().to_string()
}

/// Parse the body containing tab headers and panel contents.
pub fn parse_tabs_body(body: &str) -> Option<(Vec<TabItem>, usize)> {
    let mut tabs = Vec::new();
    let mut default_index = None;
    let mut current_title: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        // Ignore container closing marker
        if trimmed == ":::" {
            continue;
        }

        if let Some((title, is_default)) = parse_tab_header(line) {
            if let Some(prev_title) = current_title.take() {
                let content = dedent_lines(&current_lines);
                tabs.push(TabItem {
                    title: prev_title.into(),
                    content: content.into(),
                });
                current_lines.clear();
            }
            if is_default && default_index.is_none() {
                default_index = Some(tabs.len());
            }
            current_title = Some(title.to_string());
        } else if current_title.is_some() {
            current_lines.push(line);
        }
    }

    if let Some(prev_title) = current_title {
        let content = dedent_lines(&current_lines);
        tabs.push(TabItem {
            title: prev_title.into(),
            content: content.into(),
        });
    }

    if tabs.is_empty() {
        None
    } else {
        let def_idx = default_index.unwrap_or(0);
        Some((tabs, def_idx))
    }
}

/// Rewrite `:::tabs` / `:::tab` / `:::tabbed` container regions into
/// ```` ```tabs ```` fenced blocks the tabs plugin can parse.
///
/// The markdown pipeline parses block by block, so a container that spans a
/// paragraph, a fenced code block and another paragraph cannot be matched by
/// the per-node plugin: its later `@tab` sections would leak as raw text.
/// Normalizing the source up front keeps multi-tab containers intact.
pub fn normalize_tab_containers(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut fence: Option<(char, usize)> = None; // open code fence: (marker, run length)
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some((marker, len)) = fence {
            out.push_str(line);
            out.push('\n');
            let run = trimmed.chars().take_while(|c| *c == marker).count();
            if run >= len && trimmed.chars().all(|c| c == marker) {
                fence = None;
            }
            continue;
        }
        let backticks = trimmed.chars().take_while(|c| *c == '`').count();
        let tildes = trimmed.chars().take_while(|c| *c == '~').count();
        if backticks >= 3 {
            out.push_str(line);
            out.push('\n');
            fence = Some(('`', backticks));
            continue;
        }
        if tildes >= 3 {
            out.push_str(line);
            out.push('\n');
            fence = Some(('~', tildes));
            continue;
        }
        let is_open = trimmed == ":::tabs"
            || trimmed == ":::tab"
            || trimmed == ":::tabbed"
            || trimmed.starts_with(":::tabs ")
            || trimmed.starts_with(":::tab ")
            || trimmed.starts_with(":::tabbed ");
        if is_open {
            out.push_str("````tabs\n");
            for inner in lines.by_ref() {
                if inner.trim_start() == ":::" {
                    break;
                }
                out.push_str(inner);
                out.push('\n');
            }
            out.push_str("````\n");
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Parse a container block starting with `:::tabs` or `:::tab`.
pub fn parse_tabs_container(source: &str) -> Option<(Vec<TabItem>, usize)> {
    let trimmed = source.trim();
    let body = if let Some(after) = trimmed.strip_prefix(":::tabs") {
        after
    } else if let Some(after) = trimmed.strip_prefix(":::tab") {
        after
    } else {
        trimmed.strip_prefix(":::tabbed")?
    };
    parse_tabs_body(body)
}

/// Parse blockquotes containing tabs e.g. `> [!tabs]` or `> :::tabs`.
pub fn parse_tabs_blockquote(source: &str) -> Option<(Vec<TabItem>, usize)> {
    let mut unquoted = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        let stripped = match line.strip_prefix('>') {
            Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
            None => line,
        };
        unquoted.push(stripped);
    }
    let body = unquoted.join("\n");
    let trimmed = body.trim();
    if let Some(after) = trimmed.strip_prefix("[!tabs]") {
        parse_tabs_body(after)
    } else if let Some(after) = trimmed.strip_prefix("[!tab]") {
        parse_tabs_body(after)
    } else if trimmed.starts_with(":::tabs") || trimmed.starts_with(":::tab") {
        parse_tabs_container(trimmed)
    } else {
        None
    }
}

/// Byte `(offset, len)` of an mdast node in the fragment being parsed.
fn node_source_range(cx: &MarkdownParseContext<'_>, node: &Node) -> Option<(usize, usize)> {
    let position = node.position()?;
    let start = position.start.offset;
    let end = position.end.offset;
    Some((cx.offset() + start, end.saturating_sub(start)))
}

#[derive(Clone)]
pub struct TabsPlugin {
    content_extensions: MarkdownExtensions,
    state: TabsState,
}

impl MarkdownPlugin for TabsPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "tabs"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        match node {
            Node::Code(code) => {
                let lang = code
                    .lang
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                if lang == "tabs"
                    || lang == "tab"
                    || lang == "tabbed"
                    || lang.starts_with("tabs:")
                    || lang.starts_with("tab:")
                {
                    let (tabs, default_index) = parse_tabs_body(&code.value)?;
                    let source = cx.node_source(node).unwrap_or(&code.value);
                    Some(
                        MarkdownNode::new(
                            "tabs",
                            TabsData {
                                tabs,
                                default_index,
                                source_range: node_source_range(cx, node)
                                    .unwrap_or((0, source.len())),
                            },
                        )
                        .accessibility_label("Tabbed content")
                        .markdown(source),
                    )
                } else {
                    None
                }
            }
            Node::Blockquote(_) => {
                let source = cx.node_source(node)?;
                let (tabs, default_index) = parse_tabs_blockquote(source)?;
                Some(
                    MarkdownNode::new(
                        "tabs",
                        TabsData {
                            tabs,
                            default_index,
                            source_range: node_source_range(cx, node).unwrap_or((0, source.len())),
                        },
                    )
                    .accessibility_label("Tabbed content")
                    .markdown(source),
                )
            }
            Node::Paragraph(_) | Node::Html(_) => {
                let source = cx.node_source(node)?;
                let (tabs, default_index) = parse_tabs_container(source)?;
                Some(
                    MarkdownNode::new(
                        "tabs",
                        TabsData {
                            tabs,
                            default_index,
                            source_range: node_source_range(cx, node).unwrap_or((0, source.len())),
                        },
                    )
                    .accessibility_label("Tabbed content")
                    .markdown(source),
                )
            }
            _ => None,
        }
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let data = node.data::<TabsData>().expect("tabs node data");
        let block_id = content_hash(node.as_markdown());
        let active_index = self
            .state
            .get_active(block_id, data.default_index, data.tabs.len());

        let theme = cx.theme();
        let border_color = theme.border;
        let card_bg = theme.background;
        let header_bg = theme.muted.opacity(0.35);

        let active_tab = &data.tabs[active_index];
        let content_id = SharedString::from(format!("tab-body-{block_id}-{active_index}"));

        let state = self.state.clone();
        let copy_button = Clipboard::new(SharedString::from(format!("tab-copy-{block_id}")))
            .value(active_tab.content.clone())
            .tooltip("Copy tab content");

        let tab_bar = TabBar::new(SharedString::from(format!("tab-bar-{block_id}")))
            .underline()
            .selected_index(active_index)
            .children(
                data.tabs
                    .iter()
                    .map(|tab| Tab::new().label(tab.title.clone())),
            )
            .suffix(copy_button)
            .on_click(move |&new_idx, _window, cx| {
                state.set_active(block_id, new_idx, cx);
            });

        v_flex()
            .id(SharedString::from(format!("tabs-{block_id}")))
            .w_full()
            .my_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(border_color)
            .bg(card_bg)
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .bg(header_bg)
                    .border_b_1()
                    .border_color(border_color)
                    .px_2()
                    .child(tab_bar),
            )
            .child(
                div()
                    .w_full()
                    .px_4()
                    .py_3()
                    .when(!active_tab.content.is_empty(), |this| {
                        this.child(
                            TextView::markdown(content_id, active_tab.content.clone())
                                .markdown_extensions(self.content_extensions.clone())
                                .on_link_click(crate::links::on_click)
                                .w_full(),
                        )
                    }),
            )
    }
}

// -------------------------------------------------------------------------
// Math

#[derive(Debug, Clone, PartialEq, Eq)]
struct Formula {
    tex: String,
    style: MathStyle,
}

fn formula_node(tex: &str, style: MathStyle, source: &str) -> MarkdownNode {
    let name = match style {
        MathStyle::Inline => "inline-math",
        MathStyle::Display => "math",
    };
    MarkdownNode::new(
        name,
        Formula {
            tex: tex.to_string(),
            style,
        },
    )
    .text(math::shown_source(tex).to_string())
    .accessibility_label(format!("Formula: {}", math::shown_source(tex)))
    .markdown(source)
}

/// A rendered formula ready to paint.
struct PreparedMath {
    image: Arc<Image>,
    width: f32,
    height: f32,
    baseline: f32,
}

enum MathEntry {
    Pending,
    Ready(Arc<PreparedMath>),
    Failed(SharedString),
}

/// LaTeX source, style, font size (as bits) and RGBA color.
type MathKey = (String, MathStyle, u32, u32);

/// Formulas rendered for the preview, keyed by source, style, size and
/// color. Rendering runs on a background thread; the preview is asked to
/// lay out again once a formula is ready.
#[derive(Clone)]
struct MathCache {
    entries: Arc<Mutex<SizedLru<MathKey, MathEntry>>>,
    preview: WeakEntity<TextViewState>,
}

/// Computer Modern has a smaller x-height than interface sans-serif fonts;
/// scale formulas so they read at the same size as the text around them.
pub(crate) const INLINE_MATH_SCALE: f32 = 1.15;
/// Display formulas are set a little larger again, as in print.
pub(crate) const DISPLAY_MATH_SCALE: f32 = 1.3;

/// Formulas kept, least recently used dropped first, by count and by the
/// rendered SVGs' bytes.
const MATH_CACHE_LIMIT: usize = 512;
const MATH_CACHE_BYTES: usize = 32 * 1024 * 1024;

impl MathEntry {
    fn bytes(&self) -> usize {
        match self {
            MathEntry::Pending => 0,
            MathEntry::Ready(math) => math.image.bytes().len(),
            MathEntry::Failed(error) => error.len(),
        }
    }
}

enum Lookup {
    Ready(Arc<PreparedMath>),
    Waiting,
    Failed(SharedString),
}

impl MathCache {
    fn new(preview: &gpui_kit::Entity<TextViewState>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(SizedLru::new(
                MATH_CACHE_LIMIT,
                MATH_CACHE_BYTES,
            ))),
            preview: preview.downgrade(),
        }
    }

    /// The prepared formula, starting its rendering when it is new.
    fn get(&self, formula: &Formula, font_size: f32, color: Hsla, cx: &mut App) -> Lookup {
        let rgba = Rgba::from(color);
        let key = (
            formula.tex.clone(),
            formula.style,
            font_size.to_bits(),
            u32::from(rgba),
        );
        let mut entries = self.entries.lock().expect("math cache poisoned");
        match entries.get(&key) {
            Some(MathEntry::Ready(math)) => return Lookup::Ready(math.clone()),
            Some(MathEntry::Pending) => return Lookup::Waiting,
            Some(MathEntry::Failed(error)) => return Lookup::Failed(error.clone()),
            None => {}
        }
        entries.insert(key.clone(), MathEntry::Pending, 0);
        drop(entries);

        let color = format!("#{:08x}", u32::from(rgba));
        let (tex, style) = (formula.tex.clone(), formula.style);
        let task = cx
            .background_executor()
            .spawn(async move { math::render(&tex, style, font_size, &color) });
        let cache = self.clone();
        cx.spawn(async move |cx| {
            let entry = match task.await {
                Ok(svg) => {
                    // Layout snaps sizes to whole pixels; round up so the
                    // formula is never clipped and its baseline, which sits
                    // at the very bottom of formulas without descenders,
                    // stays inside the box the inline layout validates.
                    let height = svg.height.ceil();
                    MathEntry::Ready(Arc::new(PreparedMath {
                        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.svg.into_bytes())),
                        width: svg.width.ceil(),
                        height,
                        baseline: svg.baseline.clamp(0., height),
                    }))
                }
                Err(error) => MathEntry::Failed(error.into()),
            };
            let size = entry.bytes();
            cache
                .entries
                .lock()
                .expect("math cache poisoned")
                .insert(key, entry, size);
            _ = cache
                .preview
                .update(cx, |preview, cx| preview.invalidate_inline_layout(cx));
        })
        .detach();
        Lookup::Waiting
    }
}

fn math_image(math: &PreparedMath) -> gpui_kit::Img {
    img(math.image.clone())
        .object_fit(ObjectFit::Contain)
        .flex_shrink_0()
        .w(px(math.width))
        .h(px(math.height))
}

/// The formula's source, shown while it renders or when it cannot.
fn math_source(tex: &str, color: Hsla, cx: &App) -> gpui_kit::Div {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_color(color)
        .child(math::shown_source(tex).to_string())
}

/// Display math: `$$` fences, or a paragraph that is only `$$ … $$`.
struct BlockMathPlugin {
    cache: MathCache,
}

pub(crate) fn display_math_source(source: &str) -> Option<&str> {
    let body = source.trim().strip_prefix("$$")?.strip_suffix("$$")?.trim();
    (!body.is_empty()).then_some(body)
}

impl MarkdownPlugin for BlockMathPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "math"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        match node {
            Node::Code(code) if math::is_typst_math_fence(code.lang.as_deref()) => {
                Some(formula_node(
                    &math::typst_source(&code.value),
                    MathStyle::Display,
                    cx.node_source(node).unwrap_or(&code.value),
                ))
            }
            Node::Math(math) => Some(formula_node(
                &math.value,
                MathStyle::Display,
                cx.node_source(node).unwrap_or(&math.value),
            )),
            Node::Paragraph(_) => {
                let source = cx.node_source(node)?;
                let tex = display_math_source(source)?;
                Some(formula_node(tex, MathStyle::Display, source))
            }
            _ => None,
        }
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let formula = node.data::<Formula>().expect("math node data");
        let font_size = f32::from(window.text_style().font_size.to_pixels(window.rem_size()));
        let font_size = (font_size * DISPLAY_MATH_SCALE).round();
        let foreground = cx.theme().foreground;

        let content = match self.cache.get(formula, font_size, foreground, cx) {
            Lookup::Ready(math) => math_image(&math).into_any_element(),
            Lookup::Waiting => {
                math_source(&formula.tex, cx.theme().muted_foreground, cx).into_any_element()
            }
            Lookup::Failed(error) => v_flex()
                .items_center()
                .gap_1()
                .child(math_source(&formula.tex, cx.theme().danger, cx))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(error),
                )
                .into_any_element(),
        };

        div()
            .id(SharedString::from(format!(
                "math-{}",
                content_hash(&formula.tex)
            )))
            .w_full()
            .flex()
            .justify_center()
            .py_1()
            .overflow_x_scroll()
            .child(content)
    }
}

/// Inline math: `$…$`.
struct InlineMathPlugin {
    cache: MathCache,
}

/// Whether an inline `$…$` span is math. Like GitHub, prose such as
/// "costs $5 and $10" is not: the content may not start or end with
/// whitespace, and the closing dollar may not be followed by a digit.
pub(crate) fn is_inline_math(value: &str, following: Option<char>) -> bool {
    !value.is_empty()
        && !value.starts_with(char::is_whitespace)
        && !value.ends_with(char::is_whitespace)
        && !following.is_some_and(|ch| ch.is_ascii_digit())
}

impl MarkdownPlugin for InlineMathPlugin {
    fn name(&self) -> &str {
        "inline-math"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::InlineMath(math) = node else {
            return None;
        };
        let end = node.position()?.end.offset;
        let following = cx.source().get(end..).and_then(|rest| rest.chars().next());
        if !is_inline_math(&math.value, following) {
            return None;
        }
        let source = cx.node_source(node).unwrap_or(&math.value);
        Some(formula_node(&math.value, MathStyle::Inline, source))
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let formula = node.data::<Formula>()?;
        let font_size = (f32::from(context.font_size()) * INLINE_MATH_SCALE).round();
        let color = context.text_style().color;
        let lookup = self.cache.get(formula, font_size, color, cx);
        match lookup {
            Lookup::Ready(math) => {
                Some(InlineElement::new(math_image(&math)).with_baseline(px(math.baseline)))
            }
            // The preview shows the node's text until the formula is ready.
            Lookup::Waiting | Lookup::Failed(_) => None,
        }
    }
}

// -------------------------------------------------------------------------
// Mermaid diagrams

#[derive(Debug, Clone, PartialEq, Eq)]
struct MermaidSource(String);

/// A rendered diagram ready to paint.
struct PreparedDiagram {
    image: Arc<Image>,
    width: f32,
    height: f32,
}

enum DiagramEntry {
    Pending,
    Ready(Arc<PreparedDiagram>),
    Failed(SharedString),
}

enum DiagramLookup {
    Ready(Arc<PreparedDiagram>),
    Waiting,
    Failed(SharedString),
}

/// Diagrams kept, least recently used dropped first, by count and by the
/// rendered SVGs' bytes.
const DIAGRAM_CACHE_LIMIT: usize = 128;
const DIAGRAM_CACHE_BYTES: usize = 64 * 1024 * 1024;

impl DiagramEntry {
    fn bytes(&self) -> usize {
        match self {
            DiagramEntry::Pending => 0,
            DiagramEntry::Ready(diagram) => diagram.image.bytes().len(),
            DiagramEntry::Failed(error) => error.len(),
        }
    }
}
/// Diagrams are laid out for this text size and scaled with the preview's.
const DIAGRAM_FONT_SIZE: f32 = 16.;

/// Diagrams rendered for the preview, keyed by source and theme, rendered
/// on a background thread like formulas.
#[derive(Clone)]
struct DiagramCache {
    entries: Arc<Mutex<SizedLru<(u64, u64), DiagramEntry>>>,
    preview: WeakEntity<TextViewState>,
}

impl DiagramCache {
    fn new(preview: &gpui_kit::Entity<TextViewState>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(SizedLru::new(
                DIAGRAM_CACHE_LIMIT,
                DIAGRAM_CACHE_BYTES,
            ))),
            preview: preview.downgrade(),
        }
    }

    fn get(&self, source: &str, theme: DiagramTheme, cx: &mut App) -> DiagramLookup {
        let key = (content_hash(source), content_hash(&format!("{theme:?}")));
        let mut entries = self.entries.lock().expect("diagram cache poisoned");
        match entries.get(&key) {
            Some(DiagramEntry::Ready(diagram)) => return DiagramLookup::Ready(diagram.clone()),
            Some(DiagramEntry::Pending) => return DiagramLookup::Waiting,
            Some(DiagramEntry::Failed(error)) => return DiagramLookup::Failed(error.clone()),
            None => {}
        }
        entries.insert(key, DiagramEntry::Pending, 0);
        drop(entries);

        let source = source.to_string();
        let id = format!("mermaid-{:x}", key.0);
        let task = cx
            .background_executor()
            .spawn(async move { diagram::render(&source, &theme, &id) });
        let cache = self.clone();
        cx.spawn(async move |cx| {
            let entry = match task.await {
                Ok(diagram) => DiagramEntry::Ready(Arc::new(PreparedDiagram {
                    image: Arc::new(Image::from_bytes(
                        ImageFormat::Svg,
                        diagram.svg.into_bytes(),
                    )),
                    width: diagram.width,
                    height: diagram.height,
                })),
                Err(error) => DiagramEntry::Failed(error.into()),
            };
            let size = entry.bytes();
            cache
                .entries
                .lock()
                .expect("diagram cache poisoned")
                .insert(key, entry, size);
            _ = cache
                .preview
                .update(cx, |preview, cx| preview.invalidate_inline_layout(cx));
        })
        .detach();
        DiagramLookup::Waiting
    }
}

/// `color` as `#rrggbb`, composited over `background` when translucent.
fn hex_over(color: Hsla, background: Hsla) -> String {
    let (fg, bg) = (Rgba::from(color), Rgba::from(background));
    let mix = |f: f32, b: f32| ((f * fg.a + b * (1. - fg.a)).clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        mix(fg.r, bg.r),
        mix(fg.g, bg.g),
        mix(fg.b, bg.b)
    )
}

/// The diagram colors that match the app's theme.
pub(crate) fn diagram_theme(cx: &App) -> DiagramTheme {
    let theme = cx.theme();
    let background = theme.background;
    let hex = |color: Hsla| hex_over(color, background);
    // Shades are tints of the text color over the background, so every
    // theme gets light fills and clear outlines, whatever its own
    // secondary colors are tuned for.
    let tint = |amount: f32| hex(theme.foreground.opacity(amount));
    DiagramTheme {
        dark: theme.is_dark(),
        canvas: hex(background),
        surface: tint(0.045),
        surface_alt: tint(0.09),
        text: hex(theme.foreground),
        subtle_text: hex(theme.muted_foreground),
        border: tint(0.35),
        line: tint(0.55),
        note_background: hex(theme.warning.opacity(0.16)),
        note_border: hex(theme.warning.opacity(0.7)),
        error: hex(theme.danger),
        warning: hex(theme.warning),
        success: hex(theme.success),
        // Distinct hues (themes' chart colors are often shades of one),
        // softened toward the background.
        series: [
            theme.blue,
            theme.green,
            theme.yellow,
            theme.red,
            theme.magenta,
            theme.cyan,
        ]
        .map(|color| hex(color.opacity(0.75)))
        .to_vec(),
        background: hex(background),
    }
}

/// Fenced code blocks marked `mermaid` render as diagrams.
struct MermaidPlugin {
    cache: DiagramCache,
}

impl MarkdownPlugin for MermaidPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "mermaid"
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Code(code) = node else {
            return None;
        };
        if !diagram::is_mermaid(code.lang.as_deref()) {
            return None;
        }
        Some(
            MarkdownNode::new("mermaid", MermaidSource(code.value.clone()))
                .text(code.value.clone())
                .accessibility_label("Mermaid diagram")
                .markdown(cx.node_source(node).unwrap_or(&code.value)),
        )
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let source = &node.data::<MermaidSource>().expect("mermaid node data").0;
        let id = SharedString::from(format!("mermaid-{}", content_hash(source)));
        let scale = f32::from(window.rem_size()) / DIAGRAM_FONT_SIZE;
        let theme = diagram_theme(cx);

        let content = match self.cache.get(source, theme, cx) {
            DiagramLookup::Ready(diagram) => {
                let width = diagram.width * scale;
                let source = SharedString::from(source.clone());
                let open = move |window: &mut Window, cx: &mut App| {
                    window.dispatch_action(Box::new(ViewDiagram(source.clone())), cx)
                };
                let open_from_button = open.clone();
                h_flex()
                    .w_full()
                    .justify_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}-view")))
                            .group("diagram")
                            .relative()
                            .w_full()
                            .max_w(px(width))
                            .cursor_pointer()
                            .on_click(move |event, window, cx| {
                                if !event.is_right_click() {
                                    open(window, cx);
                                }
                            })
                            .child(
                                img(diagram.image.clone())
                                    .object_fit(ObjectFit::Contain)
                                    .w_full()
                                    .aspect_ratio(diagram.width / diagram.height),
                            )
                            .child(
                                // Shown on hover, so it's clear a click opens the
                                // diagram larger.
                                div()
                                    .absolute()
                                    .top_1()
                                    .right_1()
                                    .invisible()
                                    .group_hover("diagram", |style| style.visible())
                                    .child(
                                        Button::new(SharedString::from(format!("{id}-expand")))
                                            .icon(Icon::new(IconName::Maximize))
                                            .small()
                                            .outline()
                                            .tooltip("Open larger")
                                            .on_click(move |_, window, cx| {
                                                cx.stop_propagation();
                                                open_from_button(window, cx)
                                            }),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            DiagramLookup::Waiting => div()
                .h(px(48.))
                .w_full()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted.opacity(0.5))
                .into_any_element(),
            DiagramLookup::Failed(error) => v_flex()
                .w_full()
                .gap_2()
                .p_3()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().danger.opacity(0.4))
                .bg(cx.theme().danger.opacity(0.05))
                .child(
                    h_flex()
                        .items_start()
                        .gap_2()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(Icon::new(IconName::TriangleAlert).small().mt_0p5())
                        .child(div().flex_1().min_w_0().child(error)),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(source.trim_end().to_string()),
                )
                .into_any_element(),
        };
        div().id(id).w_full().py_2().child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_alerts() {
        let (kind, body) = parse_alert("> [!WARNING]\n> Back up first.\n>\n> - Really.").unwrap();
        assert_eq!(kind, AlertKind::Warning);
        assert_eq!(body, "Back up first.\n\n- Really.");

        assert_eq!(
            parse_alert("> [!tip]").unwrap(),
            (AlertKind::Tip, String::new())
        );
        // The marker must stand alone on the first line.
        assert!(parse_alert("> [!NOTE] inline text").is_none());
        assert!(parse_alert("> Just a quote").is_none());
        assert!(parse_alert("> [!UNKNOWN]\n> text").is_none());
    }

    #[test]
    fn tells_math_from_prices() {
        assert!(is_inline_math("x^2", Some(' ')));
        assert!(!is_inline_math("5 and ", Some('1')));
        assert!(!is_inline_math(" x", None));
        assert!(!is_inline_math("x", Some('0')));
        assert_eq!(display_math_source("$$ a + b $$"), Some("a + b"));
        assert_eq!(display_math_source("$$ $$"), None);
    }

    #[test]
    fn composites_translucent_colors() {
        let white: Hsla = gpui_kit::rgb(0xffffff).into();
        let black: Hsla = gpui_kit::rgb(0x000000).into();
        assert_eq!(hex_over(black, white), "#000000");
        assert_eq!(hex_over(black.opacity(0.5), white), "#808080");
        assert_eq!(hex_over(gpui_kit::rgb(0x0969da).into(), black), "#0969da");
    }

    #[test]
    fn normalizes_multitab_container() {
        let src = "intro\n\n:::tabs\n@tab A\nbody a\n\n@tab B\nbody b\n:::\n\nafter\n";
        let out = normalize_tab_containers(src);
        assert!(out.contains("````tabs"), "wrapped: {out}");
        assert!(out.contains("@tab A"));
        assert!(out.contains("@tab B"));
        assert!(out.contains("body b"));
        assert!(!out.contains(":::tabs"));
        let (tabs, _) = parse_tabs_body(
            out.split("````tabs\n")
                .nth(1)
                .unwrap()
                .split("````\n")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tabs.len(), 2);
    }

    #[test]
    fn normalizes_container_with_inner_fence() {
        let src = ":::tabs\n@tab A\n```bash\nrun a\n```\n\n@tab B\nbody\n:::\n";
        let out = normalize_tab_containers(src);
        let (tabs, _) = parse_tabs_body(
            out.split("````tabs\n")
                .nth(1)
                .unwrap()
                .split("````\n")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].content.contains("run a"));
    }

    #[test]
    fn leaves_code_blocks_alone() {
        let src = "```text\n:::tabs\n@tab A\n:::\n```\n";
        let out = normalize_tab_containers(src);
        assert!(out.contains("```text"));
        assert!(!out.contains("````tabs"));
    }

    #[test]
    fn parses_tabs_from_code_block() {
        let body = r#"=== "Rust"
fn main() {
    println!("Hello, World!");
}

===! "Python"
print("Hello, World!")

=== "Go"
fmt.Println("Hello, World!")
"#;
        let (tabs, default_index) = parse_tabs_body(body).unwrap();
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].title.as_ref(), "Rust");
        assert!(tabs[0].content.as_ref().contains("println!"));
        assert_eq!(tabs[1].title.as_ref(), "Python");
        assert!(tabs[1].content.as_ref().contains("print("));
        assert_eq!(tabs[2].title.as_ref(), "Go");
        assert_eq!(default_index, 1);
    }

    #[test]
    fn parses_tabs_container_syntax() {
        let container = r#":::tabs
@tab Tab 1
Content 1
@tab Tab 2
Content 2
:::
"#;
        let (tabs, default_index) = parse_tabs_container(container).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title.as_ref(), "Tab 1");
        assert_eq!(tabs[0].content.as_ref(), "Content 1");
        assert_eq!(tabs[1].title.as_ref(), "Tab 2");
        assert_eq!(tabs[1].content.as_ref(), "Content 2");
        assert_eq!(default_index, 0);
    }

    #[test]
    fn parses_tabs_blockquote_syntax() {
        let bq = "> [!tabs]\n> === \"First\"\n> Hello\n> === \"Second\"\n> World";
        let (tabs, default_index) = parse_tabs_blockquote(bq).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title.as_ref(), "First");
        assert_eq!(tabs[0].content.as_ref(), "Hello");
        assert_eq!(tabs[1].title.as_ref(), "Second");
        assert_eq!(tabs[1].content.as_ref(), "World");
        assert_eq!(default_index, 0);
    }

    #[test]
    fn dedents_tab_content() {
        let body = "=== \"Indented\"\n    line 1\n    line 2\n=== \"Normal\"\nline 3";
        let (tabs, _) = parse_tabs_body(body).unwrap();
        assert_eq!(tabs[0].content.as_ref(), "line 1\nline 2");
        assert_eq!(tabs[1].content.as_ref(), "line 3");
    }
}

/// Turn single newlines inside paragraphs into GFM hard breaks (`  \n`),
/// leaving fenced code blocks and blank-line paragraph splits alone.
///
/// Iterates `split_inclusive` directly — no `Vec<&str>` of every line.
#[allow(dead_code)] // exercised by unit tests; prepare uses apply_gfm_breaks_into
pub fn apply_gfm_breaks(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 32);
    apply_gfm_breaks_into(source, &mut out);
    out
}

fn apply_gfm_breaks_into(source: &str, out: &mut String) {
    let mut in_fence = false;
    let mut fence_marker: Option<char> = None;
    let mut lines = source.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        let mut fence_line = false;
        if !in_fence {
            if let Some(ch) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') {
                let count = trimmed.chars().take_while(|c| *c == ch).count();
                if count >= 3 {
                    in_fence = true;
                    fence_marker = Some(ch);
                    fence_line = true;
                }
            }
        } else if let Some(ch) = fence_marker {
            let count = trimmed.chars().take_while(|c| *c == ch).count();
            if count >= 3 && trimmed.chars().skip(count).all(|c| c.is_whitespace()) {
                in_fence = false;
                fence_marker = None;
                fence_line = true;
            }
        }
        out.push_str(body);
        if line.ends_with('\n') {
            let next_blank = lines
                .peek()
                .map(|n| n.strip_suffix('\n').unwrap_or(n).trim().is_empty())
                .unwrap_or(true);
            // Soft break → hard break unless blank line, fence marker, or already hard.
            if !in_fence
                && !fence_line
                && !body.is_empty()
                && !next_blank
                && !body.ends_with("  ")
                && !body.ends_with('\t')
            {
                out.push_str("  ");
            }
            out.push('\n');
        }
    }
}

/// Build the Markdown preview source in **one** allocation: tab-container
/// normalize, then optional GFM hard breaks, streamed into a single `String`.
/// Avoids the prior normalize→String then gfm→String double copy.
pub fn prepare_preview_markdown(source: &str, gfm_breaks: bool) -> String {
    if !gfm_breaks {
        return normalize_tab_containers(source);
    }
    // Normalize tabs first (rewrites :::tabs regions), then GFM into the
    // same final buffer via a temporary only when tabs actually changed.
    let normalized = normalize_tab_containers(source);
    if normalized.as_str() == source {
        // Common path: no tab containers — GFM straight from the original.
        let mut out = String::with_capacity(source.len() + 32);
        apply_gfm_breaks_into(source, &mut out);
        out
    } else {
        let mut out = String::with_capacity(normalized.len() + 32);
        apply_gfm_breaks_into(&normalized, &mut out);
        out
    }
}

#[cfg(test)]
mod gfm_breaks_tests {
    use super::apply_gfm_breaks;

    #[test]
    fn soft_breaks_become_hard_outside_fences() {
        let src = "Step 1\nStep 2\n\nNext para\n";
        let out = apply_gfm_breaks(src);
        assert!(out.contains("Step 1  \nStep 2"), "{out:?}");
        assert!(out.contains("\n\nNext para"), "{out:?}");
    }

    #[test]
    fn fences_are_left_alone() {
        let src = "```\nline1\nline2\n```\n";
        assert_eq!(apply_gfm_breaks(src), src);
    }
}
