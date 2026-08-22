//! Markdown rendering — parse markdown and render to ratatui styled text.
//!
//! Uses pulldown-cmark 0.11 for parsing and syntect for code block syntax highlighting.
//! Converts markdown AST events into ratatui Line/Span structures with proper styling.
//!
//! Performance: SyntaxSet and ThemeSet are loaded once via OnceLock singletons,
//! not per-frame per-code-block (which would cost 5-10ms each).
//!
//! Supported features:
//! - Headers (H1-H6) with visual block prefixes and headline color
//! - Code blocks with syntect syntax highlighting and box borders
//! - Tables with aligned columns and box-drawing borders
//! - Bold, italic, inline code, lists, links, block quotes, breaks

use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

use super::super::theme::*;

// ─── Syntect Singletons ──────────────────────────────────────────────────
// Load once and reuse across all frames. Each load takes ~5-10ms;
// without this, we'd reload per code block per frame (100+ms waste).

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn get_syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn get_theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

// ─── Table State ──────────────────────────────────────────────────────────

/// State for tracking table parsing across multiple pulldown-cmark events.
struct TableState {
    /// Column headers collected from TableHead.
    headers: Vec<String>,
    /// Row data collected from TableRow/TableCell.
    rows: Vec<Vec<String>>,
    /// Currently accumulating row cells.
    current_row: Vec<String>,
    /// Current cell content.
    current_cell: String,
    /// Whether we're inside the header row.
    in_header: bool,
}

impl TableState {
    fn new() -> Self {
        Self {
            headers: Vec::new(),
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
            in_header: false,
        }
    }
}

/// Render markdown content as a list of ratatui Lines.
pub fn render_markdown(content: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Enable all common markdown extensions for comprehensive parsing
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_SMART_PUNCTUATION);

    let parser = pulldown_cmark::Parser::new_ext(content, options);

    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_content = String::new();
    let mut in_bold = false;
    let mut in_italic = false;
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    // Table state — set when we encounter Tag::Table
    let mut table_state: Option<TableState> = None;

    for event in parser {
        match event {
            // ── Code block ──────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(kind)) => {
                flush_spans(&mut current_spans, &mut lines);
                in_code_block = true;
                code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                code_content.clear();
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                in_code_block = false;
                let code_lines = render_code_block(&code_lang, &code_content, max_width);
                lines.extend(code_lines);
                code_lang.clear();
                code_content.clear();
            }

            // ── Headings ────────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading { level, .. }) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                    // Add a visual separator line before the heading for H1 and H2
                    if matches!(
                        level,
                        pulldown_cmark::HeadingLevel::H1 | pulldown_cmark::HeadingLevel::H2
                    ) {
                        lines.push(Line::from(Span::styled(
                            "─".repeat(max_width.min(60)),
                            Style::default().fg(HEADLINE_COLOR),
                        )));
                    }
                    let prefix = match level {
                        pulldown_cmark::HeadingLevel::H1 => "█ ",
                        pulldown_cmark::HeadingLevel::H2 => "▓▓ ",
                        pulldown_cmark::HeadingLevel::H3 => "▒▒▒ ",
                        pulldown_cmark::HeadingLevel::H4 => "░░░░ ",
                        pulldown_cmark::HeadingLevel::H5 => "····· ",
                        pulldown_cmark::HeadingLevel::H6 => "······ ",
                    };
                    current_spans.push(Span::styled(
                        prefix.to_string(),
                        Style::default()
                            .fg(HEADLINE_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                    // Add spacing after heading
                    lines.push(Line::from(Span::raw("")));
                }
            }

            // ── Bold ────────────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Strong) => {
                in_bold = true;
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Strong) => {
                in_bold = false;
            }

            // ── Italic ──────────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Emphasis) => {
                in_italic = true;
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Emphasis) => {
                in_italic = false;
            }

            // ── Inline code ────────────────────────────────────────────────
            pulldown_cmark::Event::Code(text) => {
                if let Some(ts) = &mut table_state {
                    // Inside a table cell — accumulate as plain text
                    // (inline code styling is lost in table cells, but content is preserved)
                    ts.current_cell.push_str(&format!(" {} ", text));
                } else {
                    current_spans.push(Span::styled(
                        format!(" {} ", text),
                        Style::default().fg(CODE_LANG_COLOR).bg(CODE_BLOCK_BG),
                    ));
                }
            }

            // ── List items ──────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::List(_)) => {}
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::List(false)) => {}
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Item) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                    current_spans.push(Span::styled("  • ", Style::default().fg(ASSISTANT_COLOR)));
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Item) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                }
            }

            // ── Paragraph ──────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Paragraph) => {}
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Paragraph) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                    lines.push(Line::from(Span::raw("")));
                }
            }

            // ── Link ────────────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { .. }) => {
                if let Some(ts) = &mut table_state {
                    // Inside a table cell — just accumulate a link indicator
                    ts.current_cell.push('🔗');
                } else {
                    current_spans.push(Span::styled(
                        "🔗".to_string(),
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Link) => {}

            // ── Block quote ────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::BlockQuote(_)) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                    current_spans.push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::BlockQuote) => {
                if table_state.is_none() {
                    flush_spans(&mut current_spans, &mut lines);
                }
            }

            // ── Table ──────────────────────────────────────────────────────
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Table(_)) => {
                flush_spans(&mut current_spans, &mut lines);
                table_state = Some(TableState::new());
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::TableHead) => {
                if let Some(ts) = &mut table_state {
                    ts.in_header = true;
                    ts.current_row.clear();
                    ts.current_cell.clear();
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::TableHead) => {
                if let Some(ts) = &mut table_state {
                    // Only finalize current cell if it's non-empty.
                    // TagEnd::TableCell already pushed each cell into current_row,
                    // so current_cell should be empty here. The old condition
                    // `!ts.current_cell.is_empty() || !ts.current_row.is_empty()`
                    // would add an extra empty cell when current_row was non-empty
                    // but current_cell was already flushed — causing a ghost column
                    // that misaligns the table and pushes the first column outside borders.
                    if !ts.current_cell.is_empty() {
                        ts.current_row.push(ts.current_cell.trim().to_string());
                    }
                    ts.headers = ts.current_row.clone();
                    ts.current_row.clear();
                    ts.current_cell.clear();
                    ts.in_header = false;
                }
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::TableRow) => {
                if let Some(ts) = &mut table_state {
                    ts.current_row.clear();
                    ts.current_cell.clear();
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::TableRow) => {
                if let Some(ts) = &mut table_state {
                    // Same fix as TableHead: only push if current_cell is non-empty.
                    // Each TagEnd::TableCell already pushed its cell, so by the time
                    // we reach TableRow end, current_cell should be empty. Adding
                    // an extra empty cell would create a ghost column.
                    if !ts.current_cell.is_empty() {
                        ts.current_row.push(ts.current_cell.trim().to_string());
                    }
                    ts.rows.push(ts.current_row.clone());
                    ts.current_row.clear();
                    ts.current_cell.clear();
                }
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::TableCell) => {
                if let Some(ts) = &mut table_state {
                    ts.current_cell.clear();
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::TableCell) => {
                if let Some(ts) = &mut table_state {
                    ts.current_row.push(ts.current_cell.trim().to_string());
                    ts.current_cell.clear();
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Table) => {
                flush_spans(&mut current_spans, &mut lines);
                if let Some(ts) = table_state.take() {
                    let table_lines = render_table(&ts.headers, &ts.rows, max_width);
                    lines.extend(table_lines);
                }
            }

            // ── Breaks ──────────────────────────────────────────────────────
            pulldown_cmark::Event::HardBreak => {
                if let Some(ts) = &mut table_state {
                    // Inside a table cell — hard breaks become spaces
                    ts.current_cell.push(' ');
                } else {
                    flush_spans(&mut current_spans, &mut lines);
                }
            }
            pulldown_cmark::Event::SoftBreak => {
                if let Some(ts) = &mut table_state {
                    // Inside a table cell — soft breaks become spaces
                    ts.current_cell.push(' ');
                } else {
                    current_spans.push(Span::raw(" "));
                }
            }

            // ── Text content ────────────────────────────────────────────────
            pulldown_cmark::Event::Text(text) => {
                let text_str = text.to_string();
                if in_code_block {
                    code_content.push_str(&text_str);
                } else if let Some(ts) = &mut table_state {
                    // Inside a table cell — accumulate cell content
                    ts.current_cell.push_str(&text_str);
                } else {
                    let style = if in_bold && in_italic {
                        Style::default()
                            .fg(ASSISTANT_COLOR)
                            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
                    } else if in_bold {
                        Style::default()
                            .fg(ASSISTANT_COLOR)
                            .add_modifier(Modifier::BOLD)
                    } else if in_italic {
                        Style::default()
                            .fg(ASSISTANT_COLOR)
                            .add_modifier(Modifier::ITALIC)
                    } else {
                        Style::default().fg(ASSISTANT_COLOR)
                    };
                    for (i, part) in text_str.split('\n').enumerate() {
                        if i > 0 {
                            flush_spans(&mut current_spans, &mut lines);
                        }
                        if !part.is_empty() {
                            current_spans.push(Span::styled(part.to_string(), style));
                        }
                    }
                }
            }

            _ => {}
        }
    }

    flush_spans(&mut current_spans, &mut lines);

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty)",
            Style::default().fg(ASSISTANT_COLOR),
        )));
    }

    lines
}

/// Flush accumulated spans into a line.
fn flush_spans(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(spans.clone()));
        spans.clear();
    }
}

/// Truncate a span's content to a maximum visual width, appending "…" if truncated.
///
/// This prevents content lines from exceeding the bubble's inner_width,
/// which would push the right border │ off-screen and cause rendering artifacts.
fn truncate_span_to_width(content: &str, max_visual_width: usize) -> String {
    if max_visual_width <= 1 {
        return if max_visual_width == 1 {
            "…".to_string()
        } else {
            String::new()
        };
    }
    if content.width() <= max_visual_width {
        return content.to_string();
    }
    // Find the byte position where visual width reaches max_visual_width - 1
    // (leaving 1 cell for the "…" suffix)
    let mut total_width = 0;
    let mut byte_pos = 0;
    for (i, ch) in content.char_indices() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > max_visual_width - 1 {
            break;
        }
        total_width += ch_width;
        byte_pos = i + ch.len_utf8();
    }
    format!("{}…", &content[..byte_pos])
}

/// Render a code block with syntect syntax highlighting.
fn render_code_block(lang: &str, content: &str, max_width: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4);
    let mut lines = Vec::new();

    // Top border: "┌─ lang ────┐"
    // Total visual width must = inner_width + 4 (to match content/bottom lines)
    // The title includes "┌─ " prefix: structure is title_span + "─"×fill_len + "┐"
    // title_span = "┌─ {lang} " → visual width = "┌─ ".width() + lang.width() + " ".width() = 4 + lang_visual_width
    // So: (4 + lang_visual_width) + fill_len + 1 = inner_width + 4
    //     fill_len = inner_width - 1 - lang_visual_width
    let lang_display = if lang.is_empty() { "code" } else { lang };
    let lang_visual_width = lang_display.width();
    let fill_len = inner_width.saturating_sub(lang_visual_width + 1);
    lines.push(Line::from(vec![
        Span::styled(
            format!("┌─ {} ", lang_display),
            Style::default()
                .fg(CODE_BLOCK_BORDER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(fill_len), Style::default().fg(CODE_BLOCK_BORDER)),
        Span::styled("┐", Style::default().fg(CODE_BLOCK_BORDER)),
    ]));

    let highlighted_lines = syntect_highlight(lang, content);

    // Content lines: "│ " (2) + content + padding + " │" (2) = inner_width + 4
    // Truncate spans that would exceed inner_width to prevent │ from being pushed off-screen.
    for styled_line in highlighted_lines {
        let mut line_spans = vec![Span::styled("│ ", Style::default().fg(CODE_BLOCK_BORDER))];
        let mut content_width = 0;
        for span in styled_line {
            let span_width = span.content.as_ref().width();
            if content_width + span_width <= inner_width {
                content_width += span_width;
                line_spans.push(span);
            } else {
                // Truncate span to fit remaining space
                let remaining = inner_width.saturating_sub(content_width);
                if remaining > 0 {
                    let truncated = truncate_span_to_width(&span.content, remaining);
                    let truncated_width = truncated.width();
                    content_width += truncated_width;
                    line_spans.push(Span::styled(truncated, span.style));
                }
                break;
            }
        }
        let padding = inner_width.saturating_sub(content_width);
        line_spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().fg(ratatui::style::Color::Reset),
        ));
        line_spans.push(Span::styled(" │", Style::default().fg(CODE_BLOCK_BORDER)));
        lines.push(Line::from(line_spans));
    }

    // Bottom border: └──────────┘  (1 + inner_width+2 + 1 = inner_width+4)
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(CODE_BLOCK_BORDER)),
        Span::styled(
            "─".repeat(inner_width + 2),
            Style::default().fg(CODE_BLOCK_BORDER),
        ),
        Span::styled("┘", Style::default().fg(CODE_BLOCK_BORDER)),
    ]));

    lines
}

/// Apply syntect syntax highlighting using pre-loaded singletons.
fn syntect_highlight(lang: &str, content: &str) -> Vec<Vec<Span<'static>>> {
    use syntect::easy::HighlightLines;

    let ss = get_syntax_set();
    let ts = get_theme_set();
    let theme = &ts.themes["base16-mocha.dark"];

    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .or_else(|| ss.find_syntax_by_first_line(content))
        .cloned()
        .unwrap_or_else(|| ss.find_syntax_plain_text().clone());

    let mut highlighter = HighlightLines::new(&syntax, theme);
    let mut result = Vec::new();

    for line in syntect::util::LinesWithEndings::from(content) {
        let ranges = highlighter.highlight_line(line, ss).unwrap_or_default();
        let spans: Vec<Span<'static>> = ranges
            .iter()
            .map(|(style, text)| {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                let mut modifiers = Modifier::empty();
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::BOLD)
                {
                    modifiers |= Modifier::BOLD;
                }
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::ITALIC)
                {
                    modifiers |= Modifier::ITALIC;
                }
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::UNDERLINE)
                {
                    modifiers |= Modifier::UNDERLINED;
                }
                let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
                Span::styled(
                    trimmed.to_string(),
                    Style::default().fg(fg).add_modifier(modifiers),
                )
            })
            .collect();
        result.push(spans);
    }

    result
}

/// Render a table with aligned columns and box-drawing borders.
///
/// Uses Unicode box-drawing characters for borders and aligns column content
/// based on the maximum column width (including header). When the table is
/// wider than `max_width`, columns are shrunk proportionally (min 1) and the
/// total is guaranteed to fit `max_width`. Cell content that overflows its
/// (possibly shrunk) column wraps *within the column* — continuation lines are
/// rendered with the leading border + preceding columns (empty/padded) + the
/// column separator prepended, so a wide cell wraps under its own column
/// instead of spilling into the first column / wrapping at the screen edge
/// (issue #32).
fn render_table(headers: &[String], rows: &[Vec<String>], max_width: usize) -> Vec<Line<'static>> {
    if headers.is_empty() {
        return vec![Line::from(Span::styled(
            "(empty table)",
            Style::default().fg(ASSISTANT_COLOR),
        ))];
    }

    let num_cols = headers.len();

    // Desired column widths: max of header width and all row cell widths.
    let desired: Vec<usize> = (0..num_cols)
        .map(|col_idx| {
            let header_w = headers.get(col_idx).map(|h| h.width()).unwrap_or(0);
            let max_row_w = rows
                .iter()
                .map(|row| row.get(col_idx).map(|c| c.width()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            header_w.max(max_row_w)
        })
        .collect();

    // Fit the table to max_width. A rendered row is, per column, `║ ` + content(w)
    // + ` ║ ` separators + ` ║` end ⇒ overhead = 3*num_cols + 1 (verified against
    // the span layout below). Shrinking to fit guarantees no row exceeds the
    // terminal width — without it, an over-wide row wraps at the screen edge
    // from column 0 (the #32 bug at the row level).
    let overhead = 3 * num_cols + 1;
    let available = max_width.saturating_sub(overhead);
    let col_widths: Vec<usize> = if available == 0 {
        // Too narrow to lay out columns — give each the minimum so wrapping
        // still keeps content inside its own cell rather than across the row.
        vec![1; num_cols]
    } else {
        let total_desired: usize = desired.iter().sum();
        if total_desired <= available {
            desired.clone()
        } else {
            // Proportional shrink with a floor of 1, then greedily trim the
            // widest column until the sum fits `available` exactly.
            let mut ws: Vec<usize> = desired
                .iter()
                .map(|w| {
                    let scaled =
                        ((*w as f64) * (available as f64) / (total_desired as f64)) as usize;
                    scaled.max(1)
                })
                .collect();
            let mut sum: usize = ws.iter().sum();
            while sum > available {
                // Decrement the widest column that can still shrink.
                let target = ws
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| *v)
                    .map(|(i, _)| i);
                match target {
                    Some(i) if ws[i] > 1 => {
                        ws[i] -= 1;
                        sum -= 1;
                    }
                    _ => break,
                }
            }
            ws
        }
    };

    // Wrap each header + data cell to its column width (word-aware, char-split
    // for over-long words). A wrapped cell yields 1+ lines; rows render as many
    // lines as their tallest cell, with shorter cells padding continuation lines
    // to keep the separator grid aligned.
    let header_wrapped: Vec<Vec<String>> = headers
        .iter()
        .zip(col_widths.iter())
        .map(|(h, w)| wrap_cell(h, *w))
        .collect();
    let rows_wrapped: Vec<Vec<Vec<String>>> = rows
        .iter()
        .map(|row| {
            col_widths
                .iter()
                .enumerate()
                .map(|(i, w)| wrap_cell(row.get(i).map(|s| s.as_str()).unwrap_or(""), *w))
                .collect()
        })
        .collect();

    let mut lines = Vec::new();

    // Top border: ╔══════╦══════╗
    lines.push(table_border_line(&col_widths, '╔', '╦', '╗'));

    // Header row (possibly multi-line). BOLD + HEADLINE_COLOR content,
    // HEADLINE_COLOR separators.
    let header_lines = wrapped_line_count(&header_wrapped);
    for li in 0..header_lines {
        lines.push(table_content_line(&header_wrapped, li, &col_widths, true));
    }

    // Header-data separator: ╠══════╬══════╣
    lines.push(table_border_line(&col_widths, '╠', '╬', '╣'));

    // Data rows (each possibly multi-line). ASSISTANT_COLOR content,
    // CODE_BLOCK_BORDER separators.
    for row_wrapped in &rows_wrapped {
        let n = wrapped_line_count(row_wrapped);
        for li in 0..n {
            lines.push(table_content_line(row_wrapped, li, &col_widths, false));
        }
    }

    // Bottom border: ╚══════╩══════╝
    lines.push(table_border_line(&col_widths, '╚', '╩', '╝'));

    // Blank line after table
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Build a box-drawing border line (`╔═══╦═══╗` etc.) sized to `col_widths`.
/// Each column slot is `w + 2` (content + 1 space padding each side).
fn table_border_line(col_widths: &[usize], left: char, mid: char, right: char) -> Line<'static> {
    let mut spans = vec![Span::styled(
        left.to_string(),
        Style::default().fg(HEADLINE_COLOR),
    )];
    for (i, w) in col_widths.iter().enumerate() {
        spans.push(Span::styled(
            "═".repeat(*w + 2),
            Style::default().fg(HEADLINE_COLOR),
        ));
        if i < col_widths.len() - 1 {
            spans.push(Span::styled(
                mid.to_string(),
                Style::default().fg(HEADLINE_COLOR),
            ));
        }
    }
    spans.push(Span::styled(
        right.to_string(),
        Style::default().fg(HEADLINE_COLOR),
    ));
    Line::from(spans)
}

/// Build one content line of a row (header or data) at wrapped-line index `li`.
/// Each column contributes `cols[i].get(li)` (or empty on a continuation line),
/// padded to `col_widths[i]`, separated by ` ║ ` (right border ` ║`). Header
/// cells use HEADLINE_COLOR+BOLD; data cells use ASSISTANT_COLOR; header
/// separators are HEADLINE_COLOR, data separators CODE_BLOCK_BORDER — matching
/// the pre-wrap styling.
fn table_content_line(
    cols: &[Vec<String>],
    li: usize,
    col_widths: &[usize],
    is_header: bool,
) -> Line<'static> {
    let content_color = if is_header {
        HEADLINE_COLOR
    } else {
        ASSISTANT_COLOR
    };
    let sep_color = if is_header {
        HEADLINE_COLOR
    } else {
        CODE_BLOCK_BORDER
    };
    let mut spans = vec![Span::styled("║ ", Style::default().fg(sep_color))];
    for (i, w) in col_widths.iter().enumerate() {
        let chunk = cols
            .get(i)
            .and_then(|c| c.get(li))
            .map(|s| s.as_str())
            .unwrap_or("");
        let padding = w.saturating_sub(chunk.width());
        let mut style = Style::default().fg(content_color);
        if is_header {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(chunk.to_string(), style));
        spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().fg(ratatui::style::Color::Reset),
        ));
        if i < col_widths.len() - 1 {
            spans.push(Span::styled(" ║ ", Style::default().fg(sep_color)));
        }
    }
    spans.push(Span::styled(" ║", Style::default().fg(sep_color)));
    Line::from(spans)
}

/// Number of display lines a wrapped row occupies (max across its cells, min 1
/// so even an all-empty row still emits one line).
fn wrapped_line_count(cols: &[Vec<String>]) -> usize {
    cols.iter().map(|c| c.len()).max().unwrap_or(0).max(1)
}

/// Wrap `text` to fit within `width` display cells. Word-aware (splits on
/// ASCII whitespace, keeps words intact when they fit); a single word longer
/// than `width` is hard-split char by char (width-aware, so CJK/counted
/// correctly). `width == 0` yields a single empty line (the caller clamps
/// column widths to ≥ 1, so this is just a safety net).
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    // Split on ASCII whitespace while preserving it as soft break points; words
    // themselves are never broken unless they exceed `width`.
    for word in text.split(' ') {
        let word_w = word.width();
        if word_w > width {
            // The word alone overflows — flush the current line, then hard-split
            // the word char by char across as many lines as needed.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let mut piece = String::new();
            let mut piece_w = 0usize;
            for ch in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if piece_w + cw > width && !piece.is_empty() {
                    out.push(std::mem::take(&mut piece));
                    piece_w = 0;
                }
                piece.push(ch);
                piece_w += cw;
            }
            if !piece.is_empty() {
                cur = piece;
                cur_w = piece_w;
            }
        } else if cur_w == 0 {
            cur.push_str(word);
            cur_w = word_w;
        } else if cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = word_w;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn visual_width(s: &str) -> usize {
        s.width()
    }

    #[test]
    fn wrap_cell_word_aware() {
        assert_eq!(
            wrap_cell("hello world", 5),
            vec!["hello".to_string(), "world".to_string()]
        );
        // Words that fit together stay on one line.
        assert_eq!(wrap_cell("ab cd", 5), vec!["ab cd".to_string()]);
    }

    #[test]
    fn wrap_cell_hard_splits_overlong_word() {
        // width 4, word 10 chars → 3 lines (4+4+2).
        assert_eq!(
            wrap_cell("abcdefghij", 4),
            vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string()]
        );
    }

    #[test]
    fn wrap_cell_zero_width_is_empty_line() {
        assert_eq!(wrap_cell("x", 0), vec!["".to_string()]);
    }

    #[test]
    fn wrap_cell_empty_yields_one_empty_line() {
        assert_eq!(wrap_cell("", 5), vec!["".to_string()]);
    }

    #[test]
    fn table_overflow_wraps_within_column_with_separators() {
        // A single 10-char cell in column 0; max_width forces that column to
        // shrink to 4, so the cell must wrap inside its column.
        let headers = vec!["a".to_string(), "b".to_string()];
        let rows = vec![vec!["xxxxxxxxxx".to_string(), "y".to_string()]];
        let lines = render_table(&headers, &rows, 12);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // Every rendered line is a proper table line (begins with a box char),
        // never wrapping from column 0 of the screen (issue #32).
        for t in &texts {
            if t.is_empty() {
                continue;
            }
            let first = t.chars().next().unwrap();
            assert!(
                matches!(first, '║' | '╔' | '╠' | '╚'),
                "line should start with a box char, got: {t:?}"
            );
            // No line exceeds the terminal width.
            assert!(
                visual_width(t) <= 12,
                "line overflows max_width: {t:?} (w={})",
                visual_width(t)
            );
        }

        // The wide cell wraps to 3 continuation lines (chunks xxxx / xxxx /
        // xx), each carrying the `║` separator grid (so it stays inside column
        // 0, not the first column).
        let data_lines: Vec<&String> = texts
            .iter()
            .filter(|t| t.starts_with('║') && t.contains('x'))
            .collect();
        assert_eq!(
            data_lines.len(),
            3,
            "expected 3 wrapped lines for the 10-char cell"
        );
        for dl in &data_lines {
            assert!(
                dl.contains(" ║ "),
                "wrapped line must keep the column separator: {dl}"
            );
        }
    }

    #[test]
    fn table_fits_when_narrow() {
        // Many columns, tiny width — must still fit and not overflow the row.
        let headers = vec!["h1".to_string(), "h2".to_string(), "h3".to_string()];
        let rows = vec![vec![
            "longvalue1".to_string(),
            "longvalue2".to_string(),
            "longvalue3".to_string(),
        ]];
        let lines = render_table(&headers, &rows, 16);
        for line in &lines {
            let t = line_text(line);
            if t.is_empty() {
                continue;
            }
            assert!(
                visual_width(&t) <= 16,
                "overflow: {t:?} (w={})",
                visual_width(&t)
            );
        }
    }

    #[test]
    fn table_empty_headers() {
        let lines = render_table(&[], &[], 40);
        assert_eq!(line_text(&lines[0]), "(empty table)");
    }
}
