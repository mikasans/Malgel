//! Mermaid diagrams, laid out and drawn in pure Rust by merman.
//!
//! Output is "resvg-safe" SVG: labels are SVG text rather than the HTML in
//! `<foreignObject>` that browsers' Mermaid uses, so the same SVG draws in
//! the preview (GPUI renders SVG with resvg), in PDFs (Typst), as PNG for
//! Word and the clipboard, and in browsers.

use std::{sync::LazyLock, time::Duration};

use merman::{
    Engine, OperationControl, RenderOutput, RenderRequest, Renderer, SvgRequest,
    svg::{
        CssOverridePolicy, HostTheme, HostThemeAppearance, Presentation, SvgOutputPolicy,
        SvgPipelinePreset, SvgRenderOptions, ThemeRole,
    },
};

/// Fonts diagrams are set in. Layout measures text with metrics close to
/// Arial's, so an Arial-compatible font keeps labels inside their shapes:
/// Arial on Windows and macOS, Liberation Sans or Arimo on Linux.
const FONT_FAMILY: &str = "Arial, \"Liberation Sans\", Arimo, Helvetica, sans-serif";

/// Longest a diagram may take to lay out before it is abandoned, so a
/// pathological one can't tie up a thread.
const DEADLINE: Duration = Duration::from_secs(10);

/// Whether a fenced code block's info string marks a Mermaid diagram.
pub fn is_mermaid(language: Option<&str>) -> bool {
    language.is_some_and(|language| {
        let language = language.split_whitespace().next().unwrap_or_default();
        language.eq_ignore_ascii_case("mermaid") || language.eq_ignore_ascii_case("mmd")
    })
}

/// Colors a diagram is drawn with, as `#rrggbb`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagramTheme {
    pub dark: bool,
    pub canvas: String,
    pub surface: String,
    pub surface_alt: String,
    pub text: String,
    pub subtle_text: String,
    pub border: String,
    pub line: String,
    pub note_background: String,
    pub note_border: String,
    pub error: String,
    pub warning: String,
    pub success: String,
    /// Colors for data series: pie slices, mindmap branches, git branches.
    pub series: Vec<String>,
    /// Fill behind the whole diagram; without one merman paints white.
    pub background: String,
}

impl DiagramTheme {
    /// The light look of exported documents, which print on white.
    pub fn document() -> Self {
        Self {
            dark: false,
            canvas: "#ffffff".into(),
            surface: "#f6f8fa".into(),
            surface_alt: "#eaeef2".into(),
            text: "#1f2328".into(),
            subtle_text: "#59636e".into(),
            border: "#8c959f".into(),
            line: "#59636e".into(),
            note_background: "#fff8c5".into(),
            note_border: "#d4a72c".into(),
            error: "#d1242f".into(),
            warning: "#9a6700".into(),
            success: "#1a7f37".into(),
            series: [
                "#4f8fdb", "#e39a4c", "#58a55c", "#d8606a", "#9474c8", "#4fa9a7", "#c7a83e",
            ]
            .map(String::from)
            .to_vec(),
            background: "#ffffff".into(),
        }
    }

    fn host_theme(&self) -> Result<HostTheme, String> {
        let error = |err: merman::svg::PresentationError| err.to_string();
        let mut theme = HostTheme::new()
            .with_appearance(if self.dark {
                HostThemeAppearance::Dark
            } else {
                HostThemeAppearance::Light
            })
            .try_with_font_family(FONT_FAMILY)
            .map_err(error)?;
        let roles = [
            (ThemeRole::Canvas, &self.canvas),
            (ThemeRole::Surface, &self.surface),
            (ThemeRole::SurfaceAlt, &self.surface_alt),
            (ThemeRole::SurfaceMuted, &self.surface_alt),
            (ThemeRole::Text, &self.text),
            (ThemeRole::SubtleText, &self.subtle_text),
            (ThemeRole::Border, &self.border),
            (ThemeRole::Line, &self.line),
            (ThemeRole::EdgeLabelBackground, &self.canvas),
            (ThemeRole::ClusterBackground, &self.surface_alt),
            (ThemeRole::ClusterBorder, &self.border),
            (ThemeRole::NoteBackground, &self.note_background),
            (ThemeRole::NoteBorder, &self.note_border),
            (ThemeRole::NoteText, &self.text),
            (ThemeRole::ActorBackground, &self.surface),
            (ThemeRole::ActorBorder, &self.border),
            (ThemeRole::ActorText, &self.text),
            (ThemeRole::ActivationBackground, &self.surface_alt),
            (ThemeRole::ActivationBorder, &self.border),
            (ThemeRole::Error, &self.error),
            (ThemeRole::Warning, &self.warning),
            (ThemeRole::Success, &self.success),
        ];
        for (role, color) in roles {
            theme = theme.try_with_role(role, color).map_err(error)?;
        }
        if !self.series.is_empty() {
            theme = theme
                .try_with_series_palette(self.series.iter().map(String::as_str))
                .map_err(error)?;
        }
        Ok(theme)
    }
}

/// A rendered diagram.
#[derive(Debug, Clone)]
pub struct Diagram {
    /// SVG sized in pixels (CSS may still scale it down).
    pub svg: String,
    pub width: f32,
    pub height: f32,
}

/// Render `source` with `theme`. `id` scopes the diagram's styles and
/// markers, and must differ between diagrams sharing an HTML page.
pub fn render(source: &str, theme: &DiagramTheme, id: &str) -> Result<Diagram, String> {
    static ENGINE: LazyLock<Engine> = LazyLock::new(Engine::new);
    let presentation = Presentation::new()
        .with_theme(theme.host_theme()?)
        .resolve();
    let renderer = Renderer::new().with_engine(presentation.materialize_engine(ENGINE.clone()));
    let output = SvgOutputPolicy {
        preset: SvgPipelinePreset::ResvgSafe,
        css_override_policy: CssOverridePolicy::StripExistingImportant,
        root_background_color: Some(theme.background.clone()),
        ..SvgOutputPolicy::default()
    };
    let request = SvgRequest {
        pipeline: Some(output.pipeline()),
        presentation: presentation.render_policy(),
        options: SvgRenderOptions {
            diagram_id: Some(id.to_string()),
            ..SvgRenderOptions::default()
        },
        ..SvgRequest::default()
    };
    let control = OperationControl::new().with_deadline(DEADLINE);
    match renderer.render(RenderRequest::svg(source, control, request)) {
        Ok(RenderOutput::Svg(Some(svg))) => sized(&pad_edge_labels(svg.svg())),
        Ok(_) => Err("This isn’t a Mermaid diagram Malgel recognizes.".into()),
        Err(err) => Err(describe(&err.to_string())),
    }
}

/// Word merman's errors for people writing diagrams.
fn describe(error: &str) -> String {
    let error = error.trim();
    if error.contains("cancel") || error.contains("deadline") {
        return "The diagram took too long to lay out.".into();
    }
    let first_line = error.lines().next().unwrap_or(error);
    let mut message = first_line.chars().take(240).collect::<String>();
    if !message.ends_with('.') {
        message.push('.');
    }
    message
}

/// Give the SVG a pixel size from its viewBox (Mermaid sets `width="100%"`,
/// which only browsers can resolve).
fn sized(svg: &str) -> Result<Diagram, String> {
    let root_end = svg.find('>').ok_or("The diagram came out empty.")?;
    let root = &svg[..root_end];
    let view_box = attribute(root, "viewBox").ok_or("The diagram came out empty.")?;
    let numbers: Vec<f32> = view_box
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    let [_, _, width, height] = numbers[..] else {
        return Err("The diagram came out empty.".into());
    };
    if !(width > 0. && height > 0.) {
        return Err("The diagram came out empty.".into());
    }
    let mut new_root = root.to_string();
    for name in ["width", "height"] {
        if let Some(value) = attribute(&new_root, name) {
            new_root = new_root.replacen(&format!(" {name}=\"{value}\""), "", 1);
        }
    }
    new_root = new_root.replacen(
        "<svg",
        &format!("<svg width=\"{width:.2}\" height=\"{height:.2}\""),
        1,
    );
    Ok(Diagram {
        svg: format!("{new_root}{}", &svg[root_end..]),
        width,
        height,
    })
}

/// The group class merman gives an edge label's drawn background and text.
const EDGE_LABEL_GROUP: &str = "data-merman-source-classes=\"root edgeLabels";

/// Widen each edge label's background to fit its text, with a little room.
///
/// merman sizes the rect from its own font metrics, narrower than what resvg
/// draws, so the label ran past its background and the edge line showed
/// through the text ("yes" / "no" on a flowchart branch).
fn pad_edge_labels(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len() + 64);
    let mut rest = svg;
    while let Some(at) = rest.find(EDGE_LABEL_GROUP) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let group_end = tail.find("</g>").map_or(tail.len(), |end| end + 4);
        out.push_str(&pad_one_label(&tail[..group_end]));
        rest = &tail[group_end..];
    }
    out.push_str(rest);
    out
}

/// One fallback label group: its first `<rect …/>` resized around the text.
fn pad_one_label(group: &str) -> String {
    let fitted = (|| {
        let rect_start = group.find("<rect ")?;
        let rect_end = group[rect_start..].find("/>")? + rect_start + 2;
        let rect = &group[rect_start..rect_end];
        let text_start = group.find("<text ")?;
        let open_end = group[text_start..].find('>')? + text_start;
        let label_end = group[open_end..].find("</text>")? + open_end;
        let label = &group[open_end + 1..label_end];
        let style = attribute(&group[text_start..open_end], "style").unwrap_or("");
        let font_size = style
            .split(';')
            .find_map(|part| part.trim().strip_prefix("font-size:"))
            .and_then(|size| size.trim_end_matches("px").parse::<f32>().ok())
            .unwrap_or(16.);
        let number = |name| attribute(rect, name)?.parse::<f32>().ok();
        let (x, y, width, height) = (
            number("x")?,
            number("y")?,
            number("width")?,
            number("height")?,
        );
        let (new_width, new_height) = fitted_label_size(label, font_size, width, height);
        let center_x = x + width / 2.;
        let center_y = y + height / 2.;
        let mut fitted = rect.to_string();
        for (name, new) in [
            ("x", center_x - new_width / 2.),
            ("y", center_y - new_height / 2.),
            ("width", new_width),
            ("height", new_height),
        ] {
            let value = attribute(&fitted, name)?.to_string();
            fitted = fitted.replacen(
                &format!(" {name}=\"{value}\""),
                &format!(" {name}=\"{new:.2}\""),
                1,
            );
        }
        Some(format!(
            "{}{fitted}{}",
            &group[..rect_start],
            &group[rect_end..]
        ))
    })();
    fitted.unwrap_or_else(|| group.to_string())
}

/// The background for `label` at `font_size`: wide enough for the text as
/// resvg draws it (about 0.62 em per character in the sans fallback), plus
/// 4px either side, and never smaller than merman made it.
fn fitted_label_size(label: &str, font_size: f32, width: f32, height: f32) -> (f32, f32) {
    let text = label.chars().count() as f32 * font_size * 0.62;
    (width.max(text) + 8., height + 2.)
}

/// The value of `name="…"` in an element's start tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_mermaid_fences() {
        assert!(is_mermaid(Some("mermaid")));
        assert!(is_mermaid(Some("Mermaid title=x")));
        assert!(is_mermaid(Some("mmd")));
        assert!(!is_mermaid(Some("rust")));
        assert!(!is_mermaid(None));
    }

    #[test]
    fn renders_resvg_safe_svg_with_a_size() {
        let diagram = render(
            "flowchart LR\n  A[Write] --> B{Preview?}\n  B -- yes --> C[Ship]",
            &DiagramTheme::document(),
            "test-flow",
        )
        .unwrap();
        assert!(diagram.width > 50. && diagram.height > 10.);
        assert!(diagram.svg.starts_with("<svg width=\""));
        assert!(!diagram.svg.contains("<foreignObject"));
        assert!(!diagram.svg.contains("width=\"100%\""));
        assert!(diagram.svg.contains("Write") && diagram.svg.contains("Preview?"));
        assert!(diagram.svg.contains("test-flow"));
        // resvg, which GPUI draws SVG with, reads it.
        assert!(
            resvg::usvg::Tree::from_str(&diagram.svg, &resvg::usvg::Options::default()).is_ok()
        );
    }

    #[test]
    fn renders_every_common_diagram_type() {
        let sources = [
            "sequenceDiagram\n  A->>B: hi\n  B-->>A: hello",
            "classDiagram\n  class Doc {\n    +save() bool\n  }",
            "stateDiagram-v2\n  [*] --> Clean\n  Clean --> Dirty: edit",
            "erDiagram\n  A ||--o{ B : has",
            "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  Task :a1, 2026-01-01, 3d",
            "pie title Pets\n  \"Dogs\" : 3\n  \"Cats\" : 2",
            "mindmap\n  root((Malgel))\n    Editor\n    Preview",
            "gitGraph\n  commit\n  branch f\n  commit",
            "journey\n  title Day\n  section Work\n    Write: 5: Me",
            "timeline\n  2025 : Idea\n  2026 : Malgel",
        ];
        for source in sources {
            let diagram = render(source, &DiagramTheme::document(), "t");
            assert!(diagram.is_ok(), "{source}: {diagram:?}");
        }
    }

    #[test]
    fn reports_mistakes() {
        let error = render("flowchart LR\n  A -->", &DiagramTheme::document(), "t").unwrap_err();
        assert!(!error.is_empty());
        assert!(render("not a diagram", &DiagramTheme::document(), "t").is_err());
    }

    #[test]
    fn edge_labels_get_a_background_wider_than_their_text() {
        let (width, _) = fitted_label_size("yes", 16., 22.56, 24.);
        assert!(width >= 3. * 16. * 0.62 + 8., "{width}");
        let diagram = render(
            "flowchart LR\n  A[Write] -- yes --> C[Ship]",
            &DiagramTheme::document(),
            "test-label",
        )
        .unwrap();
        let group = &diagram.svg[diagram.svg.find(EDGE_LABEL_GROUP).expect("an edge label")..];
        let rect = &group[group.find("<rect ").unwrap()..];
        let rect = &rect[..rect.find("/>").unwrap()];
        let rect_width: f32 = attribute(rect, "width").unwrap().parse().unwrap();
        assert!(
            rect_width >= 37.,
            "label background {rect_width}px for 'yes'"
        );
        let x: f32 = attribute(rect, "x").unwrap().parse().unwrap();
        let text = &group[group.find("<text ").unwrap()..];
        let text_x: f32 = attribute(text, "x").unwrap().parse().unwrap();
        assert!(
            (x + rect_width / 2. - text_x).abs() < 0.1,
            "centered on the text"
        );
        assert!(
            resvg::usvg::Tree::from_str(&diagram.svg, &resvg::usvg::Options::default()).is_ok()
        );
    }
}
