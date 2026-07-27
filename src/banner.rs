//! Startup banner.
//!
//! Terminal cells are about twice as tall as they are wide, so drawing one
//! pixel per cell gives you a squashed image. The half block prints the upper
//! pixel as foreground and the lower pixel as background, yielding two pixels
//! per terminal cell.

use ratatui::prelude::*;

/// 14 wide × 16 tall → 14 columns × 8 rows once rendered.
///
/// . transparent   R hat        r hat shadow
/// S skin          s skin shade K eye
/// W beard         w beard shade
const GNOME: [&str; 16] = [
    "......RR......",
    ".....RRRR.....",
    "....RRRRRR....",
    "...RRRRRRRR...",
    "..RRRRRRRRRR..",
    ".RRRRRRRRRRRR.",
    "RRRRRRRRRRRRRR",
    ".rrrrrrrrrrrr.",
    "...SSSSSSSS...",
    "...SKSSSSKS...",
    "...SSSssSSS...",
    "..WWWWssWWWW..",
    ".WWWWWWWWWWWW.",
    "..WWWWWWWWWWw.",
    "...WWWWWWWw...",
    ".....WWWw.....",
];

const UPPER_HALF: &str = "▀";
const LOWER_HALF: &str = "▄";

fn palette(c: char) -> Option<Color> {
    Some(match c {
        'R' => Color::Rgb(198, 48, 48),
        'r' => Color::Rgb(138, 28, 28),
        'S' => Color::Rgb(240, 196, 160),
        's' => Color::Rgb(212, 158, 126),
        'K' => Color::Rgb(28, 24, 24),
        'W' => Color::Rgb(238, 238, 240),
        'w' => Color::Rgb(186, 188, 196),
        _ => return None,
    })
}

fn pixel(row: usize, col: usize) -> Option<Color> {
    GNOME.get(row)?.chars().nth(col).and_then(palette)
}

/// Render the sprite as Ratatui lines, one line per two sprite rows.
pub fn sprite() -> Vec<Line<'static>> {
    let width = GNOME[0].chars().count();
    let mut lines = Vec::with_capacity(GNOME.len() / 2);

    for pair in (0..GNOME.len()).step_by(2) {
        let mut spans = Vec::with_capacity(width);

        for col in 0..width {
            let top = pixel(pair, col);
            let bottom = pixel(pair + 1, col);
            let span = match (top, bottom) {
                (None, None) => Span::raw(" "),
                (Some(top), None) => Span::styled(UPPER_HALF, Style::new().fg(top)),
                (None, Some(bottom)) => Span::styled(LOWER_HALF, Style::new().fg(bottom)),
                (Some(top), Some(bottom)) => {
                    Span::styled(UPPER_HALF, Style::new().fg(top).bg(bottom))
                }
            };
            spans.push(span);
        }

        lines.push(Line::from(spans));
    }

    lines
}

/// Sprite on the left and session metadata on the right. The result belongs at
/// the start of the transcript, so it naturally scrolls away during a session.
pub fn banner(version: &str, model: &str, workspace: &str, sandbox: &str) -> Vec<Line<'static>> {
    let art = sprite();
    let dim = Style::new().fg(Color::DarkGray);
    let text: Vec<Vec<Span>> = vec![
        vec![
            Span::styled("GnomeAI-RS ", Style::new().bold()),
            Span::styled(format!("v{version}"), dim),
        ],
        vec![Span::styled(model.to_string(), dim)],
        vec![Span::styled(format!("sandbox: {sandbox}"), dim)],
        vec![Span::styled(workspace.to_string(), dim)],
    ];

    art.into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = line.spans;
            spans.push(Span::raw("   "));
            if index >= 2 {
                if let Some(extra) = text.get(index - 2) {
                    spans.extend(extra.iter().cloned());
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_is_rectangular() {
        let width = GNOME[0].chars().count();
        assert!(GNOME.iter().all(|row| row.chars().count() == width));
    }

    #[test]
    fn even_number_of_rows() {
        assert_eq!(GNOME.len() % 2, 0);
    }

    #[test]
    fn renders_half_the_rows() {
        assert_eq!(sprite().len(), GNOME.len() / 2);
    }

    #[test]
    fn transparent_pixels_render_as_spaces() {
        assert_eq!(sprite()[0].spans[0].content, " ");
    }

    #[test]
    fn banner_contains_metadata() {
        let rendered = banner("1.2.3", "model", "/workspace", "read-only");
        let text = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("GnomeAI-RS"));
        assert!(text.contains("model"));
        assert!(text.contains("/workspace"));
    }
}
