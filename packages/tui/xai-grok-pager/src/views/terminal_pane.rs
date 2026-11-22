/*
Purpose: Run an interactive child in a PTY and draw its terminal grid in a TUI pane.
Input: A command, a directory, pane dimensions, and terminal input events.
Output: Rendered terminal cells. The child owns all file edits.
Note: The host keeps its terminal modes. Child escape sequences only affect the emulator.
*/

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use alacritty_terminal::event::{Event as TerminalEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Color as TerminalColor, Rgb};
use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use xai_tty_utils::{ProcessGroup, ProcessScope};

use crate::theme::Theme;

const OUTPUT_QUEUE_SIZE: usize = 32;
const OUTPUT_CHUNK_BYTES: usize = 4096;

/*
Purpose: Return emulator status responses through the child input channel.
Input: The input sender and shared pane dimensions.
Output: A listener that has no effect on the host terminal state.
*/
#[derive(Clone)]
struct Listener {
    input: Sender<Vec<u8>>,
    size: Arc<AtomicU32>,
}

impl EventListener for Listener {
    /*
    Purpose: Answer terminal status queries through the child input channel.
    Input: An emulator event.
    Output: A PTY response. Host clipboard and title events have no effect.
    */
    fn send_event(&self, event: TerminalEvent) {
        let response = match event {
            TerminalEvent::PtyWrite(text) => text,
            TerminalEvent::ColorRequest(index, format) => format(default_rgb(index)),
            TerminalEvent::TextAreaSizeRequest(format) => {
                let size = self.size.load(Ordering::Relaxed);
                format(WindowSize {
                    num_cols: (size >> 16) as u16,
                    num_lines: size as u16,
                    cell_width: 0,
                    cell_height: 0,
                })
            }
            _ => return,
        };
        if self.input.send(response.into_bytes()).is_err() {
            tracing::debug!("embedded terminal input closed");
        }
    }
}

struct Size {
    cols: u16,
    rows: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/*
Purpose: Own one PTY child, its emulator, and its I/O channels.
Input: Input events and bytes from the child process.
Output: A terminal frame and the exit status when the child stops.
Note: Drop kills the process group and reaps the child outside the UI thread.
*/
pub(crate) struct TerminalPane {
    terminal: Term<Listener>,
    parser: ansi::Processor,
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn portable_pty::Child + Send>>,
    group: Option<Arc<ProcessGroup>>,
    _scope: ProcessScope,
    input: Sender<Vec<u8>>,
    output: Receiver<io::Result<Vec<u8>>>,
    size: Arc<AtomicU32>,
    pub(crate) exit_code: Option<u32>,
    pub(crate) error: Option<String>,
}

impl TerminalPane {
    /*
    Purpose: Start a command in a separate terminal and enroll its process group.
    Input: The command, arguments, directory, and initial pane size.
    Output: A terminal pane, or a launch error with the child cleaned up.
    */
    pub(crate) fn spawn(
        command: &str,
        args: &[&std::ffi::OsStr],
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let pair = native_pty_system().openpty(pty_size(cols, rows))?;
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let mut builder = CommandBuilder::new(command);
        builder.args(args);
        builder.cwd(cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        // The child owns a separate PTY and is enrolled immediately below.
        #[allow(clippy::disallowed_methods)]
        let mut child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("Could not start {command}"))?;
        drop(pair.slave);
        let scope = ProcessScope::new();
        let group = child
            .process_id()
            .context("The editor has no process ID")
            .and_then(|pid| {
                scope
                    .enroll_terminal_pid(pid)
                    .context("Could not enroll the editor process")
            });
        let group = match group {
            Ok(group) => group,
            Err(error) => {
                child.kill()?;
                child.wait()?;
                return Err(error);
            }
        };
        let (input, inputs) = mpsc::channel::<Vec<u8>>();
        let (outputs, output) = mpsc::sync_channel(OUTPUT_QUEUE_SIZE);
        let size = Arc::new(AtomicU32::new((u32::from(cols) << 16) | u32::from(rows)));
        let listener = Listener {
            input: input.clone(),
            size: size.clone(),
        };
        let terminal = Term::new(
            Config {
                scrolling_history: 1000,
                ..Config::default()
            },
            &Size { cols, rows },
            listener,
        );
        let pane = Self {
            terminal,
            parser: ansi::Processor::new(),
            master: pair.master,
            child: Some(child),
            group: Some(group),
            _scope: scope,
            input,
            output,
            size,
            exit_code: None,
            error: None,
        };
        std::thread::Builder::new()
            .name("file-editor-read".into())
            .spawn(move || read_output(&mut reader, &outputs))?;
        std::thread::Builder::new()
            .name("file-editor-write".into())
            .spawn(move || {
                for bytes in inputs {
                    if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                        tracing::debug!(%error, "embedded terminal input ended");
                        break;
                    }
                }
            })?;
        Ok(pane)
    }

    /*
    Purpose: Apply a bounded amount of output and collect the child exit status.
    Input: Output chunks that are ready now.
    Output: True when the terminal frame or status changes.
    */
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..OUTPUT_QUEUE_SIZE {
            let Ok(chunk) = self.output.try_recv() else {
                break;
            };
            match chunk {
                Ok(bytes) => self.parser.advance(&mut self.terminal, &bytes),
                Err(error) => self.error = Some(error.to_string()),
            }
            changed = true;
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exit_code = Some(status.exit_code());
                    self.child = None;
                    self.group = None;
                    changed = true;
                }
                Ok(None) => {}
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        changed
    }

    pub(crate) fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /*
    Purpose: Encode host input according to the child's active terminal modes.
    Input: A keyboard, paste, or focus event.
    Output: Bytes on the child PTY, or an error that the host can display.
    */
    pub(crate) fn handle_input(&mut self, event: &Event) -> Result<()> {
        let bytes = encode_input(event, *self.terminal.mode())?;
        if !bytes.is_empty() {
            self.input
                .send(bytes)
                .context("The editor input channel closed")?;
        }
        Ok(())
    }

    /*
    Purpose: Resize the emulator and the actual child terminal together.
    Input: The new width and height in cells.
    Output: The updated dimensions or an ioctl error.
    */
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.terminal.columns() == cols as usize && self.terminal.screen_lines() == rows as usize
        {
            return Ok(());
        }
        self.master.resize(pty_size(cols, rows))?;
        self.size
            .store((u32::from(cols) << 16) | u32::from(rows), Ordering::Relaxed);
        self.terminal.resize(Size { cols, rows });
        Ok(())
    }

    /*
    Purpose: Copy the emulator grid into the host frame without terminal side effects.
    Input: The target pane rectangle and its focus state.
    Output: Styled cells, wide glyphs, and a software cursor.
    */
    pub(crate) fn render(&self, buf: &mut Buffer, area: Rect, focused: bool) {
        let theme = Theme::current();
        for y in 0..area.height.min(self.terminal.screen_lines() as u16) {
            for x in 0..area.width.min(self.terminal.columns() as u16) {
                let cell = &self.terminal.grid()[Line(y as i32)][Column(x as usize)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let mut symbol = cell.c.to_string();
                if let Some(extra) = cell.zerowidth() {
                    symbol.extend(extra);
                }
                let mut modifiers = Modifier::empty();
                for (flag, modifier) in [
                    (Flags::BOLD, Modifier::BOLD),
                    (Flags::DIM, Modifier::DIM),
                    (Flags::ITALIC, Modifier::ITALIC),
                    (Flags::ALL_UNDERLINES, Modifier::UNDERLINED),
                    (Flags::INVERSE, Modifier::REVERSED),
                    (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
                    (Flags::HIDDEN, Modifier::HIDDEN),
                ] {
                    if cell.flags.intersects(flag) {
                        modifiers |= modifier;
                    }
                }
                let style = Style::default()
                    .fg(self.color(cell.fg, theme.text_primary))
                    .bg(self.color(cell.bg, theme.bg_base))
                    .add_modifier(modifiers);
                buf.set_stringn(
                    area.x + x,
                    area.y + y,
                    symbol,
                    (area.width - x) as usize,
                    style,
                );
            }
        }
        let cursor = self.terminal.grid().cursor.point;
        if focused
            && self.terminal.mode().contains(TermMode::SHOW_CURSOR)
            && cursor.line.0 >= 0
            && cursor.line.0 < area.height as i32
            && cursor.column.0 < area.width as usize
        {
            let cell = &mut buf[(
                area.x + cursor.column.0 as u16,
                area.y + cursor.line.0 as u16,
            )];
            cell.modifier.toggle(Modifier::REVERSED);
        }
    }

    /*
    Purpose: Resolve terminal palette changes before the default frame color.
    Input: An ANSI color and its default frame color.
    Output: A Ratatui color.
    */
    fn color(&self, color: TerminalColor, fallback: Color) -> Color {
        let index = match color {
            TerminalColor::Spec(rgb) => return Color::Rgb(rgb.r, rgb.g, rgb.b),
            TerminalColor::Indexed(index) => index as usize,
            TerminalColor::Named(name) => name as usize,
        };
        if let Some(rgb) = self.terminal.colors()[index] {
            return Color::Rgb(rgb.r, rgb.g, rgb.b);
        }
        if index < 256 {
            Color::Indexed(index as u8)
        } else {
            fallback
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(group) = self.group.take()
                && let Err(error) = group.kill()
            {
                tracing::warn!(%error, "could not kill the editor process group");
            }
            if let Err(error) = child.kill() {
                tracing::debug!(%error, "editor already stopped");
            }
            // The group is gone before reap, so its PID cannot be reused by a later scope kill.
            if let Err(error) = std::thread::Builder::new()
                .name("file-editor-reap".into())
                .spawn(move || {
                    if let Err(error) = child.wait() {
                        tracing::warn!(%error, "could not reap the editor");
                    }
                })
            {
                tracing::warn!(%error, "could not start the editor reaper");
            }
        }
    }
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/*
Purpose: Read bounded output chunks until the child terminal closes.
Input: The PTY reader and its bounded output channel.
Output: Output chunks or the terminal read error.
*/
fn read_output(reader: &mut Box<dyn Read + Send>, output: &SyncSender<io::Result<Vec<u8>>>) {
    let mut buffer = [0; OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => {
                if output.send(Ok(buffer[..count].to_vec())).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // Unix PTYs can report EIO when the slave closes.
                #[cfg(unix)]
                if error.raw_os_error() == Some(libc::EIO) {
                    return;
                }
                if output.send(Err(error)).is_err() {
                    tracing::debug!("editor output receiver closed");
                }
                return;
            }
        }
    }
}

/*
Purpose: Encode the input with the active paste and cursor-key modes.
Input: A host event and the terminal modes.
Output: Child input bytes. Resize events do not produce bytes.
*/
fn encode_input(event: &Event, modes: TermMode) -> Result<Vec<u8>> {
    if let Event::Key(key) = event
        && key.kind == KeyEventKind::Release
    {
        return Ok(Vec::new());
    }
    if let Event::Paste(text) = event {
        let text = text
            .replace('\u{1b}', "")
            .replace("\r\n", "\n")
            .replace('\n', "\r");
        return Ok(if modes.contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        });
    }
    if matches!(event, Event::FocusGained | Event::FocusLost)
        && !modes.contains(TermMode::FOCUS_IN_OUT)
    {
        return Ok(Vec::new());
    }
    if let Event::Mouse(mouse) = event {
        return encode_mouse(mouse, modes);
    }
    if matches!(event, Event::Resize(..)) {
        return Ok(Vec::new());
    }
    let mut event = event.clone();
    if let Event::Key(key) = &mut event {
        key.kind = KeyEventKind::Press;
    }
    let application_cursor = modes.contains(TermMode::APP_CURSOR)
        && matches!(&event, Event::Key(key) if key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End));
    let event: terminput::Event = event.try_into().context("Unsupported editor input event")?;
    let mut buffer = [0; 64];
    let count = event.encode(&mut buffer, terminput::Encoding::Xterm)?;
    let mut bytes = buffer[..count].to_vec();
    if application_cursor && bytes.starts_with(b"\x1b[") {
        bytes[1] = b'O';
    }
    Ok(bytes)
}

/*
Purpose: Encode relative mouse coordinates only when the child requests mouse input.
Input: A pane-relative mouse event and the child's terminal modes.
Output: A mouse report, arrow-key bytes for scroll, or no bytes.
*/
fn encode_mouse(mouse: &MouseEvent, modes: TermMode) -> Result<Vec<u8>> {
    if !modes
        .intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    {
        let code = match mouse.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            _ => return Ok(Vec::new()),
        };
        return encode_input(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), modes);
    }
    if matches!(mouse.kind, MouseEventKind::Moved) && !modes.contains(TermMode::MOUSE_MOTION) {
        return Ok(Vec::new());
    }
    if matches!(mouse.kind, MouseEventKind::Drag(_))
        && !modes.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    {
        return Ok(Vec::new());
    }
    let event: terminput::Event = Event::Mouse(*mouse).try_into()?;
    let mut buffer = [0; 64];
    if modes.contains(TermMode::SGR_MOUSE) {
        let count = event.encode(&mut buffer, terminput::Encoding::Xterm)?;
        return Ok(buffer[..count].to_vec());
    }
    if mouse.column >= 223 || mouse.row >= 223 {
        return Ok(Vec::new());
    }
    let button = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let mut code = match mouse.kind {
        MouseEventKind::Down(value) => button(value),
        MouseEventKind::Up(_) => 3,
        MouseEventKind::Drag(value) => 32 + button(value),
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    Ok(vec![
        27,
        b'[',
        b'M',
        32 + code,
        33 + mouse.column as u8,
        33 + mouse.row as u8,
    ])
}

/*
Purpose: Resolve an xterm palette index for terminal color queries.
Input: A palette index or a default-color index.
Output: The RGB color that the emulator reports to the child.
*/
fn default_rgb(index: usize) -> Rgb {
    const ANSI: [u32; 16] = [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let value = if index < 16 {
        ANSI[index]
    } else if index < 232 {
        let index = index - 16;
        let component = |value: usize| {
            if value == 0 {
                0
            } else {
                55 + value as u32 * 40
            }
        };
        (component(index / 36) << 16) | (component(index / 6 % 6) << 8) | component(index % 6)
    } else if index < 256 {
        (8 + (index as u32 - 232) * 10) * 0x010101
    } else if index == 257 {
        0x1e1e1e
    } else {
        0xd4d4d4
    };
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

#[cfg(test)]
#[path = "terminal_pane_tests.rs"]
mod tests;
