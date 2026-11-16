//! Panels: pure ratatui widget constructors over the screen line
//! builders. Rendering never touches the facade or the terminal
//! directly, so tests draw whole screens through `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, Screen};
use crate::facade::Facade;

/// Draw the full screen: header (title + attach state + run mode),
/// the current screen's lines, and a key-hint footer.
pub fn draw<F: Facade>(frame: &mut Frame, app: &App<F>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let header = Paragraph::new(format!(
        " periscope-tui — {} | editor: {} | mode: {}",
        app.screen().title(),
        app.attach().label(),
        app.run_mode().label(),
    ))
    .block(Block::default().borders(Borders::ALL).title("periscope"));
    frame.render_widget(header, chunks[0]);
    let body = screen_panel(app);
    frame.render_widget(body, chunks[1]);
    let footer = Paragraph::new(
        " Tab/1-5 screens  r refresh  j/k select  Enter run  p plan/auto  \
         a approve  x abort  t target  q quit",
    );
    frame.render_widget(footer, chunks[2]);
}

/// The body panel for the app's current screen.
pub fn screen_panel<F: Facade>(app: &App<F>) -> Paragraph<'static> {
    let lines: Vec<Line> = app.screen_lines().into_iter().map(Line::from).collect();
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(screen_title(app.screen())),
        )
        .wrap(Wrap { trim: false })
}

fn screen_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Overview => "Overview — broker & editor",
        Screen::Tree => "Tree — LSP / formatters / linters / DAP",
        Screen::Workflows => "Workflows — plan, approve, run",
        Screen::Log => "Log / Live — event stream",
        Screen::Fleet => "Fleet — trigger rules & audit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FakeFacade;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn first_screen_renders_detached_truthfully() {
        let mut app = App::new(FakeFacade::detached());
        app.refresh();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("DETACHED"), "buffer: {text}");
        assert!(text.contains("no editor is attached"), "buffer: {text}");
    }

    #[test]
    fn first_screen_renders_attached_identity() {
        let mut app = App::new(FakeFacade::attached());
        app.refresh();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("ATTACHED"), "buffer: {text}");
        assert!(text.contains("Neovim 0.11.4"), "buffer: {text}");
    }
}
