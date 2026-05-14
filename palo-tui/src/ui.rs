use palo_core::events::LogStream;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Sparkline, Table, Widget, Wrap,
    },
};
use std::collections::VecDeque;

use crate::app::{
    App, CommandMode, FocusedPane, GlobalSummary, LifecycleEntry, ResourceHistorySample,
    ServiceViewState,
};

const PANE_PADDING: Padding = Padding {
    left: 1,
    right: 1,
    top: 0,
    bottom: 0,
};
const LEFT_PANE_WIDTH_PERCENT: u16 = 30;
const LOGS_PANE_WIDTH_PERCENT: u16 = 100 - LEFT_PANE_WIDTH_PERCENT;
const CONTEXT_PANE_HEIGHT: u16 = 13;
const FOOTER_PANE_HEIGHT: u16 = 3;
const LEFT_VERTICAL_GAP_COUNT: u16 = 3;
const DETAIL_LABEL_WIDTH: usize = 10;
const SERVICE_COLUMNS_MINIMAL: [ServiceColumn; 2] = [ServiceColumn::Service, ServiceColumn::State];
const SERVICE_COLUMNS_COMPACT: [ServiceColumn; 3] = [
    ServiceColumn::Service,
    ServiceColumn::State,
    ServiceColumn::Ports,
];
const SERVICE_COLUMNS_MEDIUM: [ServiceColumn; 4] = [
    ServiceColumn::Service,
    ServiceColumn::State,
    ServiceColumn::Health,
    ServiceColumn::Ports,
];
const SERVICE_COLUMNS_WIDE: [ServiceColumn; 6] = [
    ServiceColumn::Service,
    ServiceColumn::State,
    ServiceColumn::Health,
    ServiceColumn::Cpu,
    ServiceColumn::Memory,
    ServiceColumn::Ports,
];
const SERVICE_COLUMNS_FULL: [ServiceColumn; 8] = [
    ServiceColumn::Service,
    ServiceColumn::State,
    ServiceColumn::Health,
    ServiceColumn::Cpu,
    ServiceColumn::Memory,
    ServiceColumn::Ports,
    ServiceColumn::Exit,
    ServiceColumn::Reason,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardPane {
    Services,
    Details,
    Logs,
}

#[derive(Debug, Clone, Copy)]
pub struct DashboardLayout {
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
    pub services: Rect,
    pub details: Rect,
    pub history: Rect,
    pub logs: Rect,
}

impl DashboardLayout {
    pub fn pane_at(&self, column: u16, row: u16) -> Option<DashboardPane> {
        if contains(self.services, column, row) {
            Some(DashboardPane::Services)
        } else if contains(self.details, column, row) || contains(self.history, column, row) {
            Some(DashboardPane::Details)
        } else if contains(self.logs, column, row) {
            Some(DashboardPane::Logs)
        } else {
            None
        }
    }
}

impl Widget for &App {
    /// Renders the user interface widgets.
    ///
    // This is where you add new widgets.
    // See the following resources:
    // - https://docs.rs/ratatui/latest/ratatui/widgets/index.html
    // - https://github.com/ratatui/ratatui/tree/master/examples
    fn render(self, area: Rect, buf: &mut Buffer) {
        let outer_style = Style::default().fg(Color::Gray);
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let outer = Block::bordered()
            .title("Palo")
            .border_type(BorderType::Rounded)
            .border_style(outer_style)
            .title_style(title_style);
        let inner = dashboard_content_area(area);
        outer.render(area, buf);
        let layout = dashboard_layout(inner);

        render_header(self, layout.header, buf);
        render_services(self, layout.services, buf);
        render_details(self, layout.details, buf);
        render_history(self, layout.history, buf);
        render_logs(self, layout.logs, buf);
        render_footer(self, layout.footer, buf);
    }
}

pub fn dashboard_content_area(area: Rect) -> Rect {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .inner(area)
}

pub fn dashboard_layout(area: Rect) -> DashboardLayout {
    let [left, logs] = Layout::horizontal([
        Constraint::Percentage(LEFT_PANE_WIDTH_PERCENT),
        Constraint::Percentage(LOGS_PANE_WIDTH_PERCENT),
    ])
    .spacing(1)
    .areas(area);
    let shared_height = status_and_services_height(left.height);
    let context_height = context_pane_height(left.height, shared_height);
    let [header, services, context, footer] = Layout::vertical([
        Constraint::Length(shared_height),
        Constraint::Length(shared_height),
        Constraint::Length(context_height),
        Constraint::Length(FOOTER_PANE_HEIGHT),
    ])
    .spacing(1)
    .areas(left);
    let [details, history] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(1)
            .areas(context);

    DashboardLayout {
        header,
        body: left,
        footer,
        services,
        details,
        history,
        logs,
    }
}

fn status_and_services_height(left_height: u16) -> u16 {
    left_height.saturating_sub(CONTEXT_PANE_HEIGHT + FOOTER_PANE_HEIGHT + LEFT_VERTICAL_GAP_COUNT)
        / 2
}

fn context_pane_height(left_height: u16, shared_height: u16) -> u16 {
    left_height.saturating_sub(
        shared_height.saturating_mul(2) + FOOTER_PANE_HEIGHT + LEFT_VERTICAL_GAP_COUNT,
    )
}

pub fn services_content_area(area: Rect) -> Rect {
    pane_content_area(area)
}

pub fn service_index_at_content_row(row: u16) -> Option<usize> {
    service_index_at_content_row_with_offset(row, 0)
}

pub fn service_index_at_content_row_with_offset(row: u16, viewport_start: usize) -> Option<usize> {
    row.checked_sub(1)
        .map(|row| viewport_start.saturating_add(usize::from(row)))
}

fn pane_content_area(area: Rect) -> Rect {
    Block::bordered().padding(PANE_PADDING).inner(area)
}

fn render_header(app: &App, area: Rect, buf: &mut Buffer) {
    let summary = &app.state.summary;
    let block = Block::bordered()
        .title(status_title(summary))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title_style(Style::default().fg(Color::Cyan))
        .padding(PANE_PADDING);
    let content_area = block.inner(area);

    block.render(area, buf);
    render_status_graphs(summary, &app.state.resource_history, content_area, buf);
}

fn render_services(app: &App, area: Rect, buf: &mut Buffer) {
    let block = pane_block("Services", app.state.focused_pane == FocusedPane::Services);
    let content_area = block.inner(area);

    if app.state.service_order.is_empty() {
        Paragraph::new(Line::from("No services loaded"))
            .block(block)
            .render(area, buf);
        return;
    }

    let viewport = service_viewport(app, content_area.height);
    let table_area = if viewport.scrollbar.is_some() {
        Rect {
            width: content_area.width.saturating_sub(1),
            ..content_area
        }
    } else {
        content_area
    };
    let columns = service_columns_for_width(table_area.width);
    let rows = app
        .state
        .service_order
        .iter()
        .skip(viewport.start)
        .take(viewport.visible_rows)
        .filter_map(|service_id| app.state.services.get(service_id))
        .map(|service| service_table_row(app, service, columns));

    block.render(area, buf);
    Table::new(rows, service_column_widths(columns))
        .header(service_header_row(columns))
        .column_spacing(1)
        .render(table_area, buf);

    if let Some(mut scrollbar_state) = viewport.scrollbar {
        render_vertical_scrollbar(content_area, buf, &mut scrollbar_state);
    }
}

struct ServiceViewport {
    start: usize,
    visible_rows: usize,
    scrollbar: Option<ScrollbarState>,
}

fn service_viewport(app: &App, content_height: u16) -> ServiceViewport {
    let service_count = app.state.service_order.len();
    let visible_rows = content_height.saturating_sub(1) as usize;
    if visible_rows == 0 {
        return ServiceViewport {
            start: 0,
            visible_rows,
            scrollbar: None,
        };
    }

    let selected_index = app
        .state
        .selected_service
        .as_ref()
        .and_then(|selected| {
            app.state
                .service_order
                .iter()
                .position(|service_id| service_id == selected)
        })
        .unwrap_or_default();
    let start = service_viewport_start(service_count, Some(selected_index), content_height);
    let scrollbar = (service_count > visible_rows).then(|| {
        ScrollbarState::new(service_count)
            .position(start)
            .viewport_content_length(visible_rows)
    });

    ServiceViewport {
        start,
        visible_rows,
        scrollbar,
    }
}

pub fn service_viewport_start(
    service_count: usize,
    selected_index: Option<usize>,
    content_height: u16,
) -> usize {
    let visible_rows = content_height.saturating_sub(1) as usize;
    if service_count == 0 || visible_rows == 0 || service_count <= visible_rows {
        return 0;
    }

    let selected_index = selected_index.unwrap_or_default().min(service_count - 1);
    let max_start = service_count.saturating_sub(visible_rows);
    selected_index
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start)
}

fn render_details(app: &App, area: Rect, buf: &mut Buffer) {
    let lines = if let Some(service) = app.state.selected_service() {
        service_detail_lines(service)
    } else {
        vec![Line::from("Select a service to inspect")]
    };

    Paragraph::new(lines)
        .block(pane_block(
            "Details",
            app.state.focused_pane == FocusedPane::Details,
        ))
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn render_history(app: &App, area: Rect, buf: &mut Buffer) {
    let block = pane_block("Lifecycle", app.state.focused_pane == FocusedPane::Details);
    let content_height = block.inner(area).height as usize;
    let lines = if let Some(service) = app.state.selected_service() {
        lifecycle_lines(service, content_height)
    } else {
        vec![Line::from("No service selected")]
    };

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn lifecycle_lines(service: &ServiceViewState, content_height: usize) -> Vec<Line<'static>> {
    if content_height == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    lines.push(watch_line(service));

    let history_limit = content_height.saturating_sub(lines.len());
    if history_limit == 0 {
        return lines;
    }

    if service.lifecycle_history.is_empty() {
        lines.push(Line::from("No lifecycle transitions captured yet"));
    } else {
        lines.extend(
            service
                .lifecycle_history
                .iter()
                .rev()
                .take(history_limit)
                .rev()
                .map(history_line),
        );
    }

    lines
}

fn render_logs(app: &App, area: Rect, buf: &mut Buffer) {
    let block = pane_block("Logs", app.state.focused_pane == FocusedPane::Logs);
    let content_area = block.inner(area);
    let mut scrollbar = None;
    let lines = if let Some(service) = app.state.selected_service() {
        if service.recent_logs.is_empty() {
            vec![Line::from("No logs yet")]
        } else {
            let viewport = log_viewport(
                service,
                content_area.width.saturating_sub(1),
                content_area.height,
            );
            scrollbar = viewport.scrollbar;
            viewport.lines
        }
    } else {
        vec![Line::from("No service selected")]
    };

    Paragraph::new(lines).block(block).render(area, buf);

    if let Some(mut scrollbar_state) = scrollbar {
        render_vertical_scrollbar(content_area, buf, &mut scrollbar_state);
    }
}

fn render_vertical_scrollbar(area: Rect, buf: &mut Buffer, state: &mut ScrollbarState) {
    ratatui::widgets::StatefulWidget::render(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("|"))
            .thumb_symbol("#")
            .track_style(Style::default().fg(Color::DarkGray))
            .thumb_style(Style::default().fg(Color::Cyan)),
        area,
        buf,
        state,
    );
}

#[cfg(test)]
fn visible_log_lines(
    service: &ServiceViewState,
    content_width: u16,
    content_height: u16,
) -> Vec<Line<'static>> {
    log_viewport(service, content_width, content_height).lines
}

struct LogViewport {
    lines: Vec<Line<'static>>,
    scrollbar: Option<ScrollbarState>,
}

fn log_viewport(
    service: &ServiceViewState,
    content_width: u16,
    content_height: u16,
) -> LogViewport {
    let max_rows = content_height as usize;
    if max_rows == 0 {
        return LogViewport {
            lines: Vec::new(),
            scrollbar: None,
        };
    }

    let mut rows = Vec::new();
    let mut total_rows = 0usize;
    for log in service.recent_logs.iter() {
        let wrapped = log_lines(log, content_width as usize);
        total_rows += wrapped.len();
        rows.extend(wrapped);
    }

    let start = rows.len().saturating_sub(max_rows);
    let lines = rows.into_iter().skip(start).collect::<Vec<_>>();
    let scrollbar = (total_rows > max_rows).then(|| {
        ScrollbarState::new(total_rows)
            .position(start)
            .viewport_content_length(max_rows)
    });

    LogViewport { lines, scrollbar }
}

#[cfg(test)]
fn log_scrollbar_state(
    service: &ServiceViewState,
    content_width: u16,
    content_height: u16,
) -> Option<ScrollbarState> {
    log_viewport(service, content_width, content_height).scrollbar
}

fn log_lines(log: &crate::app::LogLine, content_width: usize) -> Vec<Line<'static>> {
    if content_width == 0 {
        return Vec::new();
    }

    let stream = match log.stream {
        LogStream::Stdout => "out",
        LogStream::Stderr => "err",
    };
    let color = match log.stream {
        LogStream::Stdout => Color::White,
        LogStream::Stderr => Color::Red,
    };
    let prefix = format!("[{stream}] ");
    let continuation_prefix = " ".repeat(prefix.len());
    let prefix_style = Style::default().fg(color);

    let mut lines = Vec::new();
    for (index, segment) in log.message.split('\n').enumerate() {
        let label = if index == 0 {
            prefix.as_str()
        } else {
            continuation_prefix.as_str()
        };
        push_wrapped_log_segment(
            &mut lines,
            label,
            continuation_prefix.as_str(),
            segment,
            prefix_style,
            content_width,
        );
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_chars(&prefix, content_width),
            prefix_style,
        )));
    }

    lines
}

fn push_wrapped_log_segment(
    lines: &mut Vec<Line<'static>>,
    first_prefix: &str,
    continuation_prefix: &str,
    content: &str,
    prefix_style: Style,
    content_width: usize,
) {
    let mut content_spans = ansi_styled_log_spans(content);
    let mut prefix = first_prefix;
    loop {
        let label = truncate_chars(prefix, content_width);
        let label_width = label.chars().count();
        let available_content_width = content_width.saturating_sub(label_width);

        if content_spans.is_empty() || available_content_width == 0 {
            lines.push(Line::from(Span::styled(label, prefix_style)));
            return;
        }

        let mut line_spans = vec![Span::styled(label, prefix_style)];
        line_spans.extend(
            take_styled_chars(&mut content_spans, available_content_width)
                .into_iter()
                .map(|span| Span::styled(span.content, span.style)),
        );
        lines.push(Line::from(line_spans));

        if content_spans.is_empty() {
            return;
        }

        prefix = continuation_prefix;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledLogSpan {
    content: String,
    style: Style,
}

fn ansi_styled_log_spans(content: &str) -> VecDeque<StyledLogSpan> {
    let mut spans = VecDeque::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if let Some(csi) = remaining.strip_prefix("\x1b[") {
            if let Some((parameters, final_byte, rest)) = split_csi_sequence(csi) {
                push_styled_text(&mut spans, &mut text, style);
                if final_byte == 'm' {
                    apply_sgr_parameters(parameters, &mut style);
                }
                remaining = rest;
                continue;
            }
        }

        let Some(ch) = remaining.chars().next() else {
            break;
        };
        text.push(ch);
        remaining = &remaining[ch.len_utf8()..];
    }

    push_styled_text(&mut spans, &mut text, style);
    spans
}

fn split_csi_sequence(sequence: &str) -> Option<(&str, char, &str)> {
    for (index, byte) in sequence.bytes().enumerate() {
        if (0x40..=0x7e).contains(&byte) {
            let final_byte = byte as char;
            let rest = &sequence[index + 1..];
            return Some((&sequence[..index], final_byte, rest));
        }
    }
    None
}

fn push_styled_text(spans: &mut VecDeque<StyledLogSpan>, text: &mut String, style: Style) {
    if text.is_empty() {
        return;
    }

    spans.push_back(StyledLogSpan {
        content: std::mem::take(text),
        style,
    });
}

fn apply_sgr_parameters(parameters: &str, style: &mut Style) {
    let normalized = parameters.replace(':', ";");
    let codes = if normalized.is_empty() {
        vec![0]
    } else {
        normalized
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect::<Vec<_>>()
    };

    let mut index = 0;
    while index < codes.len() {
        let code = codes[index];
        match code {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            5 => *style = style.add_modifier(Modifier::SLOW_BLINK),
            6 => *style = style.add_modifier(Modifier::RAPID_BLINK),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            8 => *style = style.add_modifier(Modifier::HIDDEN),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            25 => *style = style.remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            28 => *style = style.remove_modifier(Modifier::HIDDEN),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(ansi_color(code - 30, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi_color(code - 40, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi_color(code - 90, true)),
            100..=107 => style.bg = Some(ansi_color(code - 100, true)),
            38 | 48 => {
                if let Some((color, consumed)) = extended_ansi_color(&codes[index + 1..]) {
                    if code == 38 {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }

        index += 1;
    }
}

fn ansi_color(value: u16, bright: bool) -> Color {
    match (value, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

fn extended_ansi_color(codes: &[u16]) -> Option<(Color, usize)> {
    match codes {
        [5, color, ..] => Some((Color::Indexed((*color).try_into().ok()?), 2)),
        [2, red, green, blue, ..] => Some((
            Color::Rgb(
                (*red).try_into().ok()?,
                (*green).try_into().ok()?,
                (*blue).try_into().ok()?,
            ),
            4,
        )),
        _ => None,
    }
}

fn take_styled_chars(spans: &mut VecDeque<StyledLogSpan>, max_chars: usize) -> Vec<StyledLogSpan> {
    let mut chunks = Vec::new();
    let mut remaining = max_chars;

    while remaining > 0 {
        let Some(span) = spans.pop_front() else {
            break;
        };
        let span_width = span.content.chars().count();
        if span_width <= remaining {
            remaining -= span_width;
            chunks.push(span);
            continue;
        }

        let (chunk, rest) = split_at_char_count(&span.content, remaining);
        chunks.push(StyledLogSpan {
            content: chunk.to_string(),
            style: span.style,
        });
        spans.push_front(StyledLogSpan {
            content: rest.to_string(),
            style: span.style,
        });
        break;
    }

    chunks
}

fn split_at_char_count(value: &str, max_chars: usize) -> (&str, &str) {
    if max_chars == 0 {
        return ("", value);
    }

    match value.char_indices().nth(max_chars) {
        Some((index, _)) => value.split_at(index),
        None => (value, ""),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn render_footer(app: &App, area: Rect, buf: &mut Buffer) {
    let block = footer_block();
    let content_width = block.inner(area).width as usize;
    let mode = match app.state.command_mode {
        CommandMode::Normal => "normal",
        CommandMode::Command => "command",
    };
    let status = match app.state.command_mode {
        CommandMode::Command => format!("cmd :{}", app.state.command_buffer),
        CommandMode::Normal => app
            .state
            .status_line
            .as_ref()
            .map(|status| status.message.clone())
            .unwrap_or_else(|| "ready".to_string()),
    };
    let controls = footer_controls(content_width);
    let fixed_width = format!(
        "mode {mode}  {controls}  errors {}",
        app.state.summary.global_error_count
    )
    .chars()
    .count();
    let status_width = content_width.saturating_sub(fixed_width + 2);
    let status = if status_width == 0 {
        String::new()
    } else {
        truncate(&status, status_width)
    };
    let footer = Paragraph::new(Line::from(format!(
        "mode {mode}  {controls}  {}  errors {}",
        status, app.state.summary.global_error_count,
    )))
    .style(Style::default().fg(Color::DarkGray))
    .block(block);
    footer.render(area, buf);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    Block::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title_style(style)
        .padding(PANE_PADDING)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceColumn {
    Service,
    State,
    Health,
    Cpu,
    Memory,
    Ports,
    Exit,
    Reason,
}

impl ServiceColumn {
    fn title(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::State => "state",
            Self::Health => "health",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Ports => "ports",
            Self::Exit => "exit",
            Self::Reason => "last event",
        }
    }
}

fn service_columns_for_width(width: u16) -> &'static [ServiceColumn] {
    if width >= 92 {
        &SERVICE_COLUMNS_FULL
    } else if width >= 70 {
        &SERVICE_COLUMNS_WIDE
    } else if width >= 48 {
        &SERVICE_COLUMNS_MEDIUM
    } else if width >= 36 {
        &SERVICE_COLUMNS_COMPACT
    } else {
        &SERVICE_COLUMNS_MINIMAL
    }
}

fn service_column_widths(columns: &[ServiceColumn]) -> Vec<Constraint> {
    columns
        .iter()
        .map(|column| match column {
            ServiceColumn::Service => Constraint::Min(12),
            ServiceColumn::State => Constraint::Length(10),
            ServiceColumn::Health => Constraint::Length(9),
            ServiceColumn::Cpu => Constraint::Length(7),
            ServiceColumn::Memory => Constraint::Length(10),
            ServiceColumn::Ports => Constraint::Length(13),
            ServiceColumn::Exit => Constraint::Length(6),
            ServiceColumn::Reason => Constraint::Min(12),
        })
        .collect()
}

fn service_header_row(columns: &[ServiceColumn]) -> Row<'static> {
    let cells = columns.iter().map(|column| {
        Cell::new(column.title()).style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
    });

    Row::new(cells)
}

fn service_table_row(
    app: &App,
    service: &ServiceViewState,
    columns: &[ServiceColumn],
) -> Row<'static> {
    let selected = app.state.selected_service.as_ref() == Some(&service.id);
    let cells = columns
        .iter()
        .map(|column| service_table_cell(*column, service, selected));

    Row::new(cells)
}

fn service_table_cell(
    column: ServiceColumn,
    service: &ServiceViewState,
    selected: bool,
) -> Cell<'static> {
    match column {
        ServiceColumn::Service => {
            let marker = if selected { ">" } else { " " };
            let style = Style::default()
                .fg(if selected {
                    Color::Yellow
                } else {
                    Color::White
                })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            Cell::new(format!("{marker} {}", service.id.as_str())).style(style)
        }
        ServiceColumn::State => Cell::new(format!("{:?}", service.lifecycle).to_lowercase())
            .style(Style::default().fg(lifecycle_color(service.lifecycle))),
        ServiceColumn::Health => Cell::new(format!("{:?}", service.health).to_lowercase())
            .style(Style::default().fg(health_color(service.health))),
        ServiceColumn::Cpu => Cell::new(format!("{:>5.1}%", service.cpu_millis as f64 / 1000.0))
            .style(Style::default().fg(Color::Yellow)),
        ServiceColumn::Memory => Cell::new(format!("{:>9}", format_bytes(service.memory_bytes)))
            .style(Style::default().fg(Color::Blue)),
        ServiceColumn::Ports => Cell::new(format_ports_compact(&service.open_ports))
            .style(Style::default().fg(Color::Cyan)),
        ServiceColumn::Exit => Cell::new(
            service
                .last_exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        ServiceColumn::Reason => {
            Cell::new(truncate(service.last_reason.as_deref().unwrap_or("-"), 32))
                .style(Style::default().fg(Color::DarkGray))
        }
    }
}

fn service_detail_lines(service: &ServiceViewState) -> Vec<Line<'static>> {
    let status = format!("{:?}", service.lifecycle).to_lowercase();
    let health = format!("{:?}", service.health).to_lowercase();
    let exit = service
        .last_exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "-".to_string());

    vec![
        detail_line("service", service.name.clone()),
        detail_line("status", status),
        detail_line("health", health),
        detail_line(
            "pid",
            service
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        detail_line("cpu", format!("{:.1}%", service.cpu_millis as f64 / 1000.0)),
        detail_line("memory", format_bytes(service.memory_bytes)),
        detail_line(
            "uptime",
            service
                .uptime
                .map(format_duration)
                .unwrap_or_else(|| "-".to_string()),
        ),
        detail_line("ports", format_ports(&service.open_ports)),
        detail_line("restarts", service.restart_count.to_string()),
        detail_line("exit", exit),
        detail_line(
            "error",
            service
                .last_error
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ),
    ]
}

struct StatusMetric {
    label: &'static str,
    value: String,
    values: Vec<u64>,
    max: Option<u64>,
    color: Color,
}

fn status_title(summary: &GlobalSummary) -> String {
    let errors = summary.global_error_count + summary.service_error_count;
    format!(
        "Status  services {}  running {}  failed {}  restarts {}  errors {}",
        summary.total_services,
        summary.running_services,
        summary.failed_services,
        summary.aggregate_restart_count,
        errors,
    )
}

fn render_status_graphs(
    summary: &GlobalSummary,
    history: &VecDeque<ResourceHistorySample>,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let metrics = status_metrics(summary, history);
    let rows = Layout::vertical([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .areas::<4>(area);

    for (metric, row) in metrics.iter().zip(rows) {
        render_status_metric(metric, row, buf);
    }
}

fn render_status_metric(metric: &StatusMetric, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let label_width = area.width.min(18);
    let [label_area, graph_area] =
        Layout::horizontal([Constraint::Length(label_width), Constraint::Min(3)]).areas(area);
    let label = Paragraph::new(vec![
        Line::from(Span::styled(
            metric.label,
            Style::default()
                .fg(metric.color)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(&metric.value, label_area.width as usize),
            Style::default().fg(Color::White),
        )),
    ]);
    label.render(label_area, buf);

    let mut graph = Sparkline::default()
        .data(metric.values.iter().copied())
        .style(Style::default().fg(metric.color));
    if let Some(max) = metric.max {
        graph = graph.max(max.max(1));
    }
    graph.render(graph_area, buf);
}

fn status_metrics(
    summary: &GlobalSummary,
    history: &VecDeque<ResourceHistorySample>,
) -> [StatusMetric; 4] {
    [
        StatusMetric {
            label: "CPU",
            value: format!("{:.1}%", summary.aggregate_cpu_millis as f64 / 1000.0),
            values: history_values(history, |sample| sample.cpu_millis),
            max: Some(100_000),
            color: Color::Yellow,
        },
        StatusMetric {
            label: "Memory",
            value: format_bytes(summary.aggregate_memory_bytes),
            values: history_values(history, |sample| sample.memory_bytes),
            max: None,
            color: Color::LightMagenta,
        },
        StatusMetric {
            label: "Disk",
            value: format_io_pair(
                summary.aggregate_disk_read_bytes,
                summary.aggregate_disk_written_bytes,
            ),
            values: history_deltas(history, |sample| sample.disk_bytes),
            max: None,
            color: Color::LightBlue,
        },
        StatusMetric {
            label: "Network",
            value: format_io_pair(
                summary.aggregate_network_rx_bytes,
                summary.aggregate_network_tx_bytes,
            ),
            values: history_deltas(history, |sample| sample.network_bytes),
            max: None,
            color: Color::Green,
        },
    ]
}

fn history_values(
    history: &VecDeque<ResourceHistorySample>,
    value: impl Fn(&ResourceHistorySample) -> u64,
) -> Vec<u64> {
    history.iter().map(value).collect()
}

fn history_deltas(
    history: &VecDeque<ResourceHistorySample>,
    value: impl Fn(&ResourceHistorySample) -> u64,
) -> Vec<u64> {
    let mut previous = None;
    history
        .iter()
        .map(|sample| {
            let current = value(sample);
            let delta = previous
                .map(|previous| current.saturating_sub(previous))
                .unwrap_or_default();
            previous = Some(current);
            delta
        })
        .collect()
}

fn watch_line(service: &ServiceViewState) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<12}", "watch"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(
            service
                .last_watch_event
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ),
    ])
}

fn detail_line(label: &str, value: String) -> Line<'static> {
    let label = format!("{label}:");
    Line::from(vec![
        Span::styled(
            format!("{label:<DETAIL_LABEL_WIDTH$}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_string()),
    ])
}

fn footer_block() -> Block<'static> {
    let style = Style::default().fg(Color::Gray);
    Block::bordered()
        .title("Controls")
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title_style(style)
        .padding(PANE_PADDING)
}

fn footer_controls(width: usize) -> &'static str {
    if width >= 92 {
        "j/k move  s/x/r service  S/X/R all  : command  q quit"
    } else if width >= 62 {
        "j/k move  s/x/r  S/X/R all  :  q quit"
    } else {
        "j/k  s/x/r  :  q"
    }
}

fn history_line(entry: &LifecycleEntry) -> Line<'static> {
    let state = format!("{:?}", entry.state).to_lowercase();
    let reason = entry.reason.as_deref().unwrap_or("-");
    Line::from(vec![
        Span::styled(
            format!("{:<12}", state),
            Style::default().fg(lifecycle_color(entry.state)),
        ),
        Span::raw(reason.to_string()),
    ])
}

fn lifecycle_color(state: palo_core::domain::LifecycleState) -> Color {
    match state {
        palo_core::domain::LifecycleState::Running => Color::Green,
        palo_core::domain::LifecycleState::Failed => Color::Red,
        palo_core::domain::LifecycleState::Restarting => Color::Yellow,
        palo_core::domain::LifecycleState::Starting => Color::Cyan,
        palo_core::domain::LifecycleState::Stopped => Color::Gray,
        palo_core::domain::LifecycleState::Built => Color::Blue,
        palo_core::domain::LifecycleState::Checked => Color::LightBlue,
        palo_core::domain::LifecycleState::Validated => Color::Magenta,
        palo_core::domain::LifecycleState::Discovered => Color::DarkGray,
    }
}

fn health_color(health: palo_core::domain::ServiceHealth) -> Color {
    match health {
        palo_core::domain::ServiceHealth::Healthy => Color::Green,
        palo_core::domain::ServiceHealth::Degraded => Color::Yellow,
        palo_core::domain::ServiceHealth::Unhealthy => Color::Red,
        palo_core::domain::ServiceHealth::Unknown => Color::Gray,
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    let visible: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{visible}…")
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_optional_bytes(bytes: Option<u64>) -> String {
    bytes.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

fn format_io_pair(read_bytes: Option<u64>, written_bytes: Option<u64>) -> String {
    if read_bytes.is_none() && written_bytes.is_none() {
        return "-".to_string();
    }

    format!(
        "R {} W {}",
        format_optional_bytes(read_bytes),
        format_optional_bytes(written_bytes)
    )
}

fn format_ports(ports: &[u16]) -> String {
    if ports.is_empty() {
        return "-".to_string();
    }

    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_ports_compact(ports: &[u16]) -> String {
    match ports {
        [] => "-".to_string(),
        [port] => port.to_string(),
        [first, second] => format!("{first},{second}"),
        [first, second, ..] => format!("{first},{second},..."),
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::LogLine;
    use palo_core::domain::{
        DEFAULT_SERVICE_LOG_RETENTION, LifecycleState, ServiceHealth, ServiceId,
    };
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn log_lines_wrap_unbroken_messages_to_content_width() {
        let log = LogLine {
            stream: LogStream::Stdout,
            message: "abcdefghijklmnopqrstuvwxyz".to_string(),
        };

        let lines = log_lines(&log, 12);

        assert_eq!(line_text(&lines[0]), "[out] abcdef");
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| line_text(line).chars().count() <= 12)
        );
    }

    #[test]
    fn visible_log_lines_are_limited_to_log_pane_height() {
        let service = service_with_logs([LogLine {
            stream: LogStream::Stderr,
            message: "abcdefghijklmnopqrstuvwxyz".to_string(),
        }]);

        let lines = visible_log_lines(&service, 12, 3);

        assert_eq!(lines.len(), 3);
        assert!(
            lines
                .iter()
                .all(|line| line_text(line).chars().count() <= 12)
        );
        assert_eq!(line_text(lines.last().unwrap()), "      yz");
    }

    #[test]
    fn overflowing_logs_expose_bottom_aligned_scrollbar_state() {
        let service = service_with_logs((0..5).map(|index| LogLine {
            stream: LogStream::Stdout,
            message: format!("line {index}"),
        }));

        let scrollbar =
            log_scrollbar_state(&service, 20, 3).expect("overflowing logs should have a scrollbar");

        assert_eq!(scrollbar.get_position(), 2);
    }

    #[test]
    fn dashboard_layout_spans_logs_and_matches_status_to_services() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = dashboard_layout(area);

        assert_eq!(layout.header.height, layout.services.height);
        assert_eq!(
            layout.services.y,
            layout.header.y + layout.header.height + 1
        );
        assert_eq!(layout.services.x, layout.details.x);
        assert_eq!(
            layout.details.y,
            layout.services.y + layout.services.height + 1
        );
        assert_eq!(layout.details.y, layout.history.y);
        assert_eq!(layout.details.height, layout.history.height);
        assert!(layout.details.x < layout.history.x);
        assert_eq!(
            layout.footer.y,
            layout.details.y + layout.details.height + 1
        );
        assert_eq!(layout.logs.y, area.y);
        assert_eq!(layout.logs.height, area.height);
        assert!(layout.logs.x > layout.history.x);
        assert_eq!(
            layout.pane_at(layout.history.x, layout.history.y),
            Some(DashboardPane::Details)
        );
        assert_eq!(
            layout.pane_at(layout.logs.x, layout.logs.y),
            Some(DashboardPane::Logs)
        );
    }

    #[test]
    fn explicit_newlines_continue_inside_the_same_log_window() {
        let log = LogLine {
            stream: LogStream::Stdout,
            message: "first\nsecond line".to_string(),
        };

        let lines = log_lines(&log, 17);

        assert_eq!(line_text(&lines[0]), "[out] first");
        assert_eq!(line_text(&lines[1]), "      second line");
    }

    #[test]
    fn log_lines_render_ansi_sgr_colors_as_styles() {
        let log = LogLine {
            stream: LogStream::Stdout,
            message: "\x1b[31mred\x1b[0m plain \x1b[38;5;45mindexed\x1b[0m".to_string(),
        };

        let lines = log_lines(&log, 40);

        assert_eq!(line_text(&lines[0]), "[out] red plain indexed");
        assert_eq!(lines[0].spans[1].content.as_ref(), "red");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[2].style.fg, None);
        assert_eq!(lines[0].spans[3].content.as_ref(), "indexed");
        assert_eq!(lines[0].spans[3].style.fg, Some(Color::Indexed(45)));
    }

    #[test]
    fn log_lines_wrap_ansi_colored_messages_by_visible_width() {
        let log = LogLine {
            stream: LogStream::Stdout,
            message: "\x1b[32mabcdefghi\x1b[0m".to_string(),
        };

        let lines = log_lines(&log, 12);

        assert_eq!(line_text(&lines[0]), "[out] abcdef");
        assert_eq!(line_text(&lines[1]), "      ghi");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn service_table_uses_columns_that_fit_common_pane_widths() {
        for width in [24, 33, 44, 70, 92] {
            let columns = service_columns_for_width(width);

            assert!(
                minimum_service_table_width(columns) <= width,
                "{columns:?} should fit width {width}"
            );
        }
    }

    #[test]
    fn service_content_row_zero_is_the_table_header() {
        assert_eq!(service_index_at_content_row(0), None);
        assert_eq!(service_index_at_content_row(1), Some(0));
        assert_eq!(service_index_at_content_row(2), Some(1));
    }

    #[test]
    fn service_content_rows_include_scroll_offset() {
        assert_eq!(service_index_at_content_row_with_offset(0, 4), None);
        assert_eq!(service_index_at_content_row_with_offset(1, 4), Some(4));
        assert_eq!(service_index_at_content_row_with_offset(2, 4), Some(5));
    }

    #[test]
    fn service_viewport_keeps_selected_service_visible() {
        assert_eq!(service_viewport_start(10, Some(0), 4), 0);
        assert_eq!(service_viewport_start(10, Some(2), 4), 0);
        assert_eq!(service_viewport_start(10, Some(3), 4), 1);
        assert_eq!(service_viewport_start(10, Some(9), 4), 7);
        assert_eq!(service_viewport_start(3, Some(2), 4), 0);
    }

    #[test]
    fn detail_lines_keep_label_and_value_separated() {
        let line = detail_line("service", "api".to_string());

        assert_eq!(line_text(&line), "service:  api");
    }

    #[test]
    fn service_details_show_restart_count() {
        let mut service = service_with_logs([]);
        service.restart_count = 3;
        service.last_reason = Some("restarted service `api`".to_string());

        let detail_lines = service_detail_lines(&service);
        let detail_text = detail_lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(detail_text.contains(&"restarts: 3".to_string()));
        assert!(
            detail_text
                .iter()
                .all(|line| !line.contains("restarted service `api`"))
        );
    }

    #[test]
    fn service_details_show_open_ports() {
        let mut service = service_with_logs([]);
        service.open_ports = vec![3000, 8080];

        let detail_lines = service_detail_lines(&service);
        let detail_text = detail_lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(detail_text.contains(&"ports:    3000, 8080".to_string()));
    }

    #[test]
    fn service_table_shows_service_ports() {
        assert_eq!(format_ports_compact(&[3000]), "3000");
        assert_eq!(format_ports_compact(&[3000, 8080]), "3000,8080");
        assert_eq!(format_ports_compact(&[3000, 8080, 9000]), "3000,8080,...");
    }

    #[test]
    fn disk_and_network_history_uses_activity_deltas() {
        let history = VecDeque::from([
            ResourceHistorySample {
                cpu_millis: 0,
                memory_bytes: 0,
                disk_bytes: 100,
                network_bytes: 10,
            },
            ResourceHistorySample {
                cpu_millis: 0,
                memory_bytes: 0,
                disk_bytes: 150,
                network_bytes: 10,
            },
        ]);

        assert_eq!(
            history_deltas(&history, |sample| sample.disk_bytes),
            vec![0, 50]
        );
        assert_eq!(
            history_deltas(&history, |sample| sample.network_bytes),
            vec![0, 0]
        );
    }

    #[test]
    fn lifecycle_lines_include_watch_status() {
        let mut service = service_with_logs([]);
        service.last_watch_event = Some("file changed: src/main.rs".to_string());

        let lines = lifecycle_lines(&service, 3);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(text[0], "watch       file changed: src/main.rs");
    }

    #[test]
    fn status_metrics_include_operational_graph_data() {
        let summary = GlobalSummary {
            total_services: 4,
            running_services: 2,
            failed_services: 1,
            unhealthy_services: 1,
            transitioning_services: 1,
            aggregate_restart_count: 3,
            aggregate_cpu_millis: 12_500,
            aggregate_memory_bytes: 4_096,
            aggregate_disk_read_bytes: Some(1_024),
            aggregate_disk_written_bytes: Some(2_048),
            aggregate_network_rx_bytes: None,
            aggregate_network_tx_bytes: None,
            open_port_count: 2,
            service_error_count: 1,
            global_error_count: 1,
        };

        let history = VecDeque::from([
            ResourceHistorySample {
                cpu_millis: 2_500,
                memory_bytes: 1_024,
                disk_bytes: 1_024,
                network_bytes: 0,
            },
            ResourceHistorySample {
                cpu_millis: 12_500,
                memory_bytes: 4_096,
                disk_bytes: 3_072,
                network_bytes: 0,
            },
        ]);
        let title = status_title(&summary);
        let metrics = status_metrics(&summary, &history);
        let labels = metrics
            .iter()
            .map(|metric| metric.label)
            .collect::<Vec<_>>();
        let colors = metrics
            .iter()
            .map(|metric| metric.color)
            .collect::<Vec<_>>();

        assert!(title.contains("services 4"));
        assert!(title.contains("running 2"));
        assert!(title.contains("restarts 3"));
        assert!(title.contains("errors 2"));
        assert!(!title.contains("ports"));
        assert_eq!(labels, vec!["CPU", "Memory", "Disk", "Network"]);
        assert_eq!(metrics[0].value, "12.5%");
        assert_eq!(metrics[1].value, "4.0 KiB");
        assert_eq!(metrics[2].value, "R 1.0 KiB W 2.0 KiB");
        assert_eq!(metrics[3].value, "-");
        assert_eq!(metrics[2].values, vec![0, 2_048]);
        assert_eq!(metrics[3].values, vec![0, 0]);
        assert_eq!(colors.len(), 4);
        assert!(
            colors
                .iter()
                .enumerate()
                .all(|(index, color)| colors.iter().skip(index + 1).all(|other| color != other))
        );
    }

    fn service_with_logs(logs: impl IntoIterator<Item = LogLine>) -> ServiceViewState {
        ServiceViewState {
            id: ServiceId::new("api"),
            name: "API".to_string(),
            lifecycle: LifecycleState::Running,
            health: ServiceHealth::Healthy,
            last_reason: None,
            restart_count: 0,
            last_exit_code: None,
            last_error: None,
            pid: Some(42),
            uptime: Some(Duration::from_secs(5)),
            cpu_millis: 0,
            memory_bytes: 0,
            open_ports: Vec::new(),
            disk_read_bytes: None,
            disk_written_bytes: None,
            network_rx_bytes: None,
            network_tx_bytes: None,
            last_watch_event: None,
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
            recent_logs: logs.into_iter().collect::<VecDeque<_>>(),
            lifecycle_history: VecDeque::new(),
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn minimum_service_table_width(columns: &[ServiceColumn]) -> u16 {
        let column_width = columns
            .iter()
            .map(|column| match column {
                ServiceColumn::Service => 12,
                ServiceColumn::State => 10,
                ServiceColumn::Health => 9,
                ServiceColumn::Cpu => 7,
                ServiceColumn::Memory => 10,
                ServiceColumn::Ports => 9,
                ServiceColumn::Exit => 6,
                ServiceColumn::Reason => 12,
            })
            .sum::<u16>();
        let spacing = columns.len().saturating_sub(1) as u16;

        column_width + spacing
    }
}
