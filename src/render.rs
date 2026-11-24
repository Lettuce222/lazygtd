use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const HEADING: Style = Style::new()
    .fg(Color::Rgb(122, 162, 247))
    .add_modifier(Modifier::BOLD);
const ACCENT: Style = Style::new().fg(Color::Rgb(125, 207, 255));
const MUTED: Style = Style::new().fg(Color::Rgb(169, 177, 214));
const CODE: Style = Style::new().fg(Color::Rgb(158, 206, 106));

/// Format the spec's Markdown subset for the terminal. No URI is opened and no
/// control character from the input is emitted. Tables use visible columns,
/// not bytes or scalar counts, and wrap cells independently.
pub(crate) fn markdown(source: &str, inner_width: u16) -> Vec<Line<'static>> {
    let source: String = source
        .chars()
        .filter(|&c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let lines: Vec<_> = source.split_terminator('\n').collect();
    let mut out = Vec::new();
    let mut index = 0;
    let mut fence = None;
    while let Some(&line) = lines.get(index) {
        if let Some((mark, count)) = fence {
            if let Some((c, n, info)) = marker(line)
                && c == mark
                && n >= count
                && info.trim().is_empty()
            {
                fence = None;
            } else {
                out.push(Line::styled(line.replace('\t', "    "), CODE));
            }
            index += 1;
            continue;
        }
        if let Some((mark, count, info)) = marker(line) {
            fence = Some((mark, count));
            let info = info.trim();
            if !info.is_empty() {
                out.push(Line::styled(info.to_owned(), MUTED));
            }
            index += 1;
            continue;
        }
        let table_header = cells(line);
        if let Some(header) = table_header
            && let Some(separator) = lines.get(index + 1).and_then(|s| cells(s))
            && header.len() == separator.len()
            && separator.iter().all(|cell| align(cell).is_some())
        {
            let alignments: Vec<_> = separator.iter().filter_map(|s| align(s)).collect();
            index += 2;
            let mut table = vec![header];
            while let Some(row) = lines.get(index).and_then(|s| cells(s)) {
                if row.len() != alignments.len() {
                    break;
                }
                table.push(row);
                index += 1;
            }
            render_table(&table, &alignments, inner_width.into(), &mut out);
            continue;
        }
        let trim = line.trim_start();
        if let Some((prefix, rest)) = trim.split_once(' ')
            && (1..=6).contains(&prefix.len())
            && prefix.bytes().all(|b| b == b'#')
        {
            out.push(Line::from(styled_inline(rest.trim(), HEADING)));
        } else if !trim.is_empty()
            && lines.get(index + 1).is_some_and(|next| {
                let next = next.trim();
                !next.is_empty()
                    && (next.bytes().all(|b| b == b'=') || next.bytes().all(|b| b == b'-'))
            })
        {
            out.push(Line::from(styled_inline(trim, HEADING)));
            index += 1;
        } else if line.starts_with("    ") || line.starts_with('\t') {
            let text = line
                .strip_prefix("    ")
                .or_else(|| line.strip_prefix('\t'))
                .unwrap_or(line);
            out.push(Line::styled(text.replace('\t', "    "), CODE));
        } else if let Some((prefix, rest)) = list_item(line) {
            let mut spans = vec![Span::styled(prefix, ACCENT)];
            spans.extend(styled_inline(rest, Style::default()));
            out.push(Line::from(spans));
        } else {
            out.push(Line::from(styled_inline(line, Style::default())));
        }
        index += 1;
    }
    out
}

fn marker(line: &str) -> Option<(char, usize, &str)> {
    let s = line.trim_start_matches(' ');
    if line.len() - s.len() > 3 {
        return None;
    }
    let mark = s.chars().next()?;
    if !matches!(mark, '`' | '~') {
        return None;
    }
    let count = s.bytes().take_while(|&b| b == mark as u8).count();
    (count >= 3).then(|| (mark, count, &s[count..]))
}

fn list_item(line: &str) -> Option<(String, &str)> {
    let trim = line.trim_start_matches(' ');
    let indent = " ".repeat(line.len() - trim.len());
    for mark in ["- ", "+ ", "* "] {
        if let Some(rest) = trim.strip_prefix(mark) {
            let (mark, rest) = if let Some(rest) = rest.strip_prefix("[ ] ") {
                ("□ ", rest)
            } else if let Some(rest) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                ("✓ ", rest)
            } else {
                ("• ", rest)
            };
            return Some((format!("{indent}{mark}"), rest));
        }
    }
    let digits = trim.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits)
        && (trim[digits..].starts_with(". ") || trim[digits..].starts_with(") "))
    {
        return Some((
            format!("{indent}{}. ", &trim[..digits]),
            &trim[digits + 2..],
        ));
    }
    None
}

fn push(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
    } else {
        spans.push(Span::styled(text.to_owned(), style));
    }
}

fn styled_inline(mut input: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    while !input.is_empty() {
        // Skip the common non-Markdown region in one pass.
        let size = input
            .find(['`', '[', '<', '\\', '\t'])
            .unwrap_or(input.len());
        if size != 0 {
            push(&mut spans, &input[..size], base);
            input = &input[size..];
            continue;
        }
        if input.starts_with('\\')
            && let Some(c) = input.get(1..).and_then(|s| s.chars().next())
            && c.is_ascii_punctuation()
        {
            let end = 1 + c.len_utf8();
            push(&mut spans, &input[1..end], base);
            input = &input[end..];
            continue;
        }
        if input.starts_with('`') {
            let n = input.bytes().take_while(|&b| b == b'`').count();
            if let Some(end) = closing_ticks(&input[n..], n) {
                push(&mut spans, &input[n..n + end], base.patch(CODE));
                input = &input[n + end + n..];
                continue;
            }
            push(&mut spans, &input[..n], base);
            input = &input[n..];
            continue;
        }
        if input.starts_with('[')
            && let Some((end, label, url)) = link(input)
        {
            if label != url {
                push(&mut spans, label, base);
                push(&mut spans, " (", base);
                push(&mut spans, url, base.patch(ACCENT));
                push(&mut spans, ")", base);
            } else {
                push(&mut spans, url, base.patch(ACCENT));
            }
            input = &input[end..];
            continue;
        }
        if input.starts_with('<')
            && let Some(end) = input[1..].find(['>', '<', ' '])
            && input.as_bytes().get(end + 1) == Some(&b'>')
            && (input[1..end + 1].contains("://") || input[1..end + 1].starts_with("mailto:"))
        {
            push(&mut spans, &input[1..end + 1], base.patch(ACCENT));
            input = &input[end + 2..];
            continue;
        }
        let c = input.chars().next().expect("nonempty");
        let n = c.len_utf8();
        push(
            &mut spans,
            if c == '\t' { "    " } else { &input[..n] },
            base,
        );
        input = &input[n..];
    }
    spans
}

fn closing_ticks(mut s: &str, n: usize) -> Option<usize> {
    let mut offset = 0;
    while let Some(start) = s.find('`') {
        let count = s[start..].bytes().take_while(|&b| b == b'`').count();
        if count == n {
            return Some(offset + start);
        }
        s = &s[start + count..];
        offset += start + count;
    }
    None
}

fn link(s: &str) -> Option<(usize, &str, &str)> {
    let close = s[1..].find([']', '['])? + 1;
    if !s[close..].starts_with("](") {
        return None;
    }
    let tail = &s[close + 2..];
    let start = tail.len() - tail.trim_start_matches(' ').len();
    let tail = &tail[start..];
    let depth_end = if let Some(angle) = tail.strip_prefix('<') {
        angle.find('>')? + 2
    } else {
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in tail.char_indices() {
            match c {
                ')' if depth == 0 => {
                    end = Some(i);
                    break;
                }
                ')' => depth -= 1,
                '(' => depth += 1,
                ' ' | '\t' if depth == 0 => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        end?
    };
    let dest = &tail[..depth_end];
    let url = dest
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(dest);
    if url.is_empty() || url.contains(['\t', '\n']) {
        return None;
    }
    let after = &tail[depth_end..];
    let rest = after.trim_start_matches(' ');
    let end = if rest.starts_with(')') {
        after.len() - rest.len() + 1
    } else if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let tail = &rest[1..];
        let title_end = tail.find(quote)? + 2;
        let paren = rest[title_end..].trim_start_matches(' ');
        if !paren.starts_with(')') {
            return None;
        }
        after.len() - paren.len() + 1
    } else {
        return None;
    };
    Some((close + 2 + start + depth_end + end, &s[1..close], url))
}

fn cells(line: &str) -> Option<Vec<String>> {
    if !line.contains('|') {
        return None;
    }
    let line = line.trim();
    let mut raw = line.strip_prefix('|').unwrap_or(line);
    // A terminal pipe is a delimiter unless backslash-escaped.
    if let Some(before) = raw.strip_suffix('|')
        && before.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 0
    {
        raw = before;
    }
    let mut rows = vec![String::new()];
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            rows.last_mut()?.push('|');
        } else if c == '|' {
            if rows.len() == 16 {
                return None;
            }
            rows.push(String::new());
        } else {
            rows.last_mut()?.push(c);
        }
    }
    (rows.len() >= 2).then(|| rows.into_iter().map(|s| s.trim().to_owned()).collect())
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn align(cell: &str) -> Option<Align> {
    let marks = cell.trim_matches(':');
    if marks.len() < 3 || !marks.bytes().all(|b| b == b'-') || cell.len() > marks.len() + 2 {
        return None;
    }
    Some(match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => Align::Center,
        (_, true) => Align::Right,
        _ => Align::Left,
    })
}

fn wrap_cell(line: &Line<'static>, width: usize) -> Vec<(usize, Vec<Span<'static>>)> {
    let mut rows = vec![(0, Vec::new())];
    for g in line.styled_graphemes(Style::default()) {
        let w = Span::raw(g.symbol).width();
        if rows.last().is_some_and(|r| r.0 > 0 && r.0 + w > width) {
            rows.push((0, Vec::new()));
        }
        let row = rows.last_mut().expect("seeded");
        row.0 += w;
        push(&mut row.1, g.symbol, g.style);
    }
    rows
}

fn render_table(
    rows: &[Vec<String>],
    alignments: &[Align],
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let n = alignments.len();
    let formatted: Vec<Vec<Line<'static>>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            row.iter()
                .map(|s| {
                    Line::from(styled_inline(
                        s,
                        if i == 0 { HEADING } else { Style::default() },
                    ))
                })
                .collect()
        })
        .collect();
    // Side-by-side columns need two visible cells each plus a three-cell separator.
    let rule_width = 3 * (n - 1);
    let available = width.saturating_sub(rule_width);
    if available < n * 2 {
        for (i, row) in formatted.iter().enumerate() {
            if i == 0 {
                continue;
            }
            for (label, value) in formatted[0].iter().zip(row) {
                let mut spans = label.spans.clone();
                push(&mut spans, ": ", MUTED);
                spans.extend(value.spans.clone());
                out.push(Line::from(spans));
            }
            out.push(Line::default());
        }
        if rows.len() == 1 {
            out.extend(formatted.into_iter().flatten());
        }
        return;
    }
    let wishes: Vec<_> = (0..n)
        .map(|j| {
            formatted
                .iter()
                .map(|row| row[j].width())
                .max()
                .unwrap_or(2)
                .max(2)
        })
        .collect();
    let mut widths = vec![2; n];
    let mut budget = available - n * 2;
    while budget > 0 {
        let mut changed = false;
        for (width, wish) in widths.iter_mut().zip(&wishes) {
            if *width < *wish && budget > 0 {
                *width += 1;
                budget -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (index, row) in formatted.iter().enumerate() {
        let cells: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(r, w)| wrap_cell(r, *w))
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for k in 0..height {
            let mut spans = Vec::new();
            for (j, cell) in cells.iter().enumerate() {
                if j > 0 {
                    push(&mut spans, " │ ", MUTED);
                }
                let w = cell.get(k).map_or(0, |line| line.0);
                let pad = widths[j].saturating_sub(w);
                let left = match alignments[j] {
                    Align::Left => 0,
                    Align::Center => pad / 2,
                    Align::Right => pad,
                };
                push(&mut spans, &" ".repeat(left), Style::default());
                if let Some(line) = cell.get(k) {
                    spans.extend(line.1.clone());
                }
                push(&mut spans, &" ".repeat(pad - left), Style::default());
            }
            out.push(Line::from(spans));
        }
        if index == 0 {
            out.push(Line::styled(
                widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─"),
                MUTED,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn fence_protects_markdown_and_multiple_links_show_destinations() {
        let input = "# 手順\n- [ ] [検索](https://example.invalid/a_(b) \"タイトル\") と <https://example.invalid/b>\n\n~~~sh\n# コメント\n[not a link](file:///tmp/x)\n~~~\n\u{1b}完了";
        let lines = markdown(input, 80);
        assert_eq!(text(&lines[0]), "手順");
        assert_eq!(lines[0].spans[0].style, HEADING);
        assert_eq!(
            text(&lines[1]),
            "□ 検索 (https://example.invalid/a_(b)) と https://example.invalid/b"
        );
        assert_eq!(text(&lines[4]), "# コメント");
        assert_eq!(lines[4].style, CODE);
        assert_eq!(text(&lines[5]), "[not a link](file:///tmp/x)");
        assert_eq!(text(&lines[6]), "完了");
    }

    #[test]
    fn table_tracks_japanese_display_columns_with_wrapping_and_escaped_pipes() {
        let input = "| 項目 | 説明 |\n| --- | ---: |\n| 水 | 東京と大阪 |\n| 大きな机 | a\\|b |";
        let lines = markdown(input, 17);
        let border = lines
            .iter()
            .filter_map(|line| {
                let s = text(line);
                let (before, _) = s.split_once('│')?;
                Some(Span::raw(before).width())
            })
            .collect::<Vec<_>>();
        assert!(border.len() >= 3);
        assert!(border.iter().all(|&column| column == border[0]));
        assert!(lines.iter().all(|line| line.width() <= 17));
        assert!(lines.iter().any(|l| text(l).ends_with("a|b")));
        let narrow = markdown(input, 5);
        assert!(narrow.iter().any(|l| text(l) == "項目: 水"));
        assert!(!narrow.iter().any(|l| text(l).contains('│')));
    }
    #[test]
    fn boxed_paragraph_does_not_wrap_or_shift_the_japanese_table_separator() {
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            widgets::{Block, Paragraph, Widget, Wrap},
        };
        let area = Rect::new(0, 0, 19, 8);
        let lines = markdown(
            "項目 | 説明\n--- | ---\n水 | あいうえおかきく\nかきく | 低い机",
            17,
        );
        let expected: Vec<u16> = lines
            .iter()
            .filter_map(|line| {
                text(line)
                    .split_once('│')
                    .map(|(left, _)| Span::raw(left).width() as u16 + 1)
            })
            .collect();
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines)
            .block(Block::bordered())
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);
        let painted: Vec<_> = (1..area.height - 1)
            .filter_map(|y| (1..area.width - 1).find(|&x| buffer[(x, y)].symbol() == "│"))
            .collect();
        assert_eq!(painted, expected);
        assert!(painted.len() >= 3);
    }
}
