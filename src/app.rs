use crate::core::{
    AppEnv, SavedDirectory, ShortcutLauncherInputSourceGuard, Workspace, WorkspacePaneLayout,
    WorkspaceTabLayout,
};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    env,
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const DOUBLE_CLICK_MS: u64 = 350;
const MIN_WIDTH: u16 = 36;
/// Terminal size at which the shell can spare columns for a decorative margin.
/// Deliberately independent of `MIN_WIDTH`: a terminal near the minimum needs
/// every column for content.
const SHELL_MARGIN_MIN_WIDTH: u16 = 62;
const SHELL_MARGIN_MIN_HEIGHT: u16 = 21;
/// Dialogs shrink with the terminal but stop here so their labels stay legible.
const DIALOG_MIN_WIDTH: u16 = 34;
const DIALOG_MIN_HEIGHT: u16 = 9;
const MIN_HEIGHT: u16 = 15;
const MAIN_LIST_WIDTH: u16 = 24;
/// Narrowest layout preview that still renders a readable pane map.
const PREVIEW_MIN_WIDTH: u16 = 30;
/// Narrowest quick settings column that fits its longest line unwrapped.
const QUICK_SETTINGS_MIN_WIDTH: u16 = 28;
/// Rows the workspace list keeps for itself once the preview moves below it.
const STACKED_LIST_MIN_ROWS: u16 = 4;
/// Rule, tab header, and the shortest pane map worth drawing.
const STACKED_PREVIEW_MAP_MIN_ROWS: u16 = 5;
/// The detail area splits evenly, so both halves must clear the wider minimum.
const WORKSPACE_COLUMNS_MIN_WIDTH: u16 = MAIN_LIST_WIDTH
    + 2 * if PREVIEW_MIN_WIDTH > QUICK_SETTINGS_MIN_WIDTH {
        PREVIEW_MIN_WIDTH
    } else {
        QUICK_SETTINGS_MIN_WIDTH
    };
const GRID_CELL_MIN_WIDTH: u16 = 6;
const GRID_CELL_MAX_WIDTH: u16 = 26;
const GRID_CELL_GAP: u16 = 2;
const GRID_MAX_COLUMNS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiExit {
    None,
    Cd(PathBuf),
    ReplaceSplit(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserMode {
    Workspace,
    Directory,
}

pub fn run_tui(env: &mut AppEnv) -> Result<TuiExit> {
    let mut app = App::new(env.list_workspaces()?, env.list_directories()?);
    let (_shortcut_input_source_guard, input_source_warning) =
        match ShortcutLauncherInputSourceGuard::activate_for_tui() {
            Ok(guard) => (Some(guard), None),
            Err(error) => (
                None,
                Some(format!(
                    "ASCII input source switch failed; letter shortcuts may not work: {error}"
                )),
            ),
        };
    let mut terminal = TerminalSession::start()?;

    if let Some(warning) = input_source_warning {
        app.set_error(warning);
    }

    loop {
        terminal.draw(|frame| draw(frame, &mut app, env))?;

        if let Some(expiry) = app.status_expiry
            && Instant::now() >= expiry
        {
            app.clear_status();
        }

        if !event::poll(Duration::from_millis(60)).context("failed to poll terminal events")? {
            continue;
        }

        match event::read().context("failed to read terminal event")? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match app.handle_key(key, env)? {
                    Action::None => {}
                    Action::Quit => break,
                    Action::Refresh => match (env.list_workspaces(), env.list_directories()) {
                        (Ok(workspaces), Ok(directories)) => {
                            app.reload_workspaces(workspaces);
                            app.reload_directories(directories);
                            app.set_success("Reloaded list");
                        }
                        (Err(error), _) | (_, Err(error)) => app.set_error(error.to_string()),
                    },
                    Action::LaunchWorkspace(name) => {
                        let warning = launch_workspace_from_tui(&mut terminal, env, &name)?;
                        drop(terminal);
                        if let Some(warning) = warning {
                            eprintln!("warning: {warning}");
                        }
                        return Ok(TuiExit::None);
                    }
                    Action::SaveWorkspace(name) => {
                        terminal.suspend()?;
                        let result = env.save_current_window(&name);
                        terminal.resume()?;

                        match result {
                            Ok(path) => {
                                app.reset_dialogs();
                                app.reload_workspaces(env.list_workspaces()?);
                                app.select_name(&name);
                                app.set_success(format!(
                                    "Saved workspace \"{name}\" to {}",
                                    display_path(&path)
                                ));
                            }
                            Err(error) => app.set_error(error.to_string()),
                        }
                    }
                    Action::RenameWorkspace(old_name, new_name) => {
                        match env.rename_workspace(&old_name, &new_name) {
                            Ok(_) => {
                                app.reset_dialogs();
                                app.reload_workspaces(env.list_workspaces()?);
                                app.select_name(&new_name);
                                app.set_success(format!(
                                    "Renamed workspace \"{old_name}\" to \"{new_name}\""
                                ));
                            }
                            Err(error) => app.set_error(error.to_string()),
                        }
                    }
                    Action::EditWorkspace(name) => {
                        terminal.suspend()?;
                        let result = env.open_in_editor(&name);
                        terminal.resume()?;

                        match result {
                            Ok(()) => {
                                app.reload_workspaces(env.list_workspaces()?);
                                app.select_name(&name);
                                app.set_success(format!("Closed editor for \"{name}\""));
                            }
                            Err(error) => app.set_error(error.to_string()),
                        }
                    }
                    Action::DeleteWorkspace(name) => match env.remove_workspace(&name) {
                        Ok(_) => {
                            app.reset_dialogs();
                            app.reload_workspaces(env.list_workspaces()?);
                            app.set_success(format!("Removed workspace \"{name}\""));
                        }
                        Err(error) => app.set_error(error.to_string()),
                    },
                    Action::ReplaceDirectory(path) => match env.validate_directory_target(&path) {
                        Ok(()) => {
                            drop(terminal);
                            return Ok(TuiExit::ReplaceSplit(path));
                        }
                        Err(error) => app.set_error(error.to_string()),
                    },
                    Action::SaveDirectory(name, path) => match env.save_directory(&name, &path) {
                        Ok(saved_path) => {
                            app.reset_dialogs();
                            app.reload_directories(env.list_directories()?);
                            app.select_name(&name);
                            app.set_success(format!(
                                "Saved directory \"{name}\" to {}",
                                display_path(&saved_path)
                            ));
                        }
                        Err(error) => app.set_error(error.to_string()),
                    },
                    Action::RenameDirectory(old_name, new_name) => {
                        match env.rename_directory(&old_name, &new_name) {
                            Ok(_) => {
                                app.reset_dialogs();
                                app.reload_directories(env.list_directories()?);
                                app.select_name(&new_name);
                                app.set_success(format!(
                                    "Renamed directory \"{old_name}\" to \"{new_name}\""
                                ));
                            }
                            Err(error) => app.set_error(error.to_string()),
                        }
                    }
                    Action::DeleteDirectory(name) => match env.remove_directory(&name) {
                        Ok(_) => {
                            app.reset_dialogs();
                            app.reload_directories(env.list_directories()?);
                            app.set_success(format!("Removed directory \"{name}\""));
                        }
                        Err(error) => app.set_error(error.to_string()),
                    },
                    Action::ToggleCloseTab => match env.set_close_tab(!env.config.close_tab) {
                        Ok(()) => {
                            app.set_success(format!("close_tab = {}", env.close_tab_display()))
                        }
                        Err(error) => app.set_error(error.to_string()),
                    },
                    Action::SetGhosttyShortcut(shortcut) => {
                        match env.set_ghostty_shortcut(&shortcut) {
                            Ok(result) => {
                                app.dialog = app.shortcut_return_dialog.clone();
                                app.shortcut_input.clear();
                                if result.sync.shortcut == "off" {
                                    if result.status
                                        == crate::core::GhosttyShortcutApplyStatus::ManualConfigRemovalRequired
                                    {
                                        app.set_success(
                                            "Shortcut file removed. Also remove the include from your Ghostty config source.",
                                        );
                                    } else {
                                        app.set_success(
                                            "Ghostty-local shortcut disabled. Run `gtab init` to restore Cmd+G.",
                                        );
                                    }
                                } else if result.status
                                    == crate::core::GhosttyShortcutApplyStatus::ManualConfigRequired
                                {
                                    app.set_success(
                                        "Shortcut file updated. Add the include to your Ghostty config source, then rebuild.",
                                    );
                                } else {
                                    app.set_success(format!(
                                    "Ghostty-local shortcut saved as {}. Reload Ghostty config to apply it.",
                                    result.sync.shortcut
                                ));
                                }
                            }
                            Err(error) => app.set_error(error.to_string()),
                        }
                    }
                }
            }
            Event::Mouse(mouse) => match app.handle_mouse(mouse, env)? {
                Action::None => {}
                Action::LaunchWorkspace(name) => {
                    let warning = launch_workspace_from_tui(&mut terminal, env, &name)?;
                    drop(terminal);
                    if let Some(warning) = warning {
                        eprintln!("warning: {warning}");
                    }
                    return Ok(TuiExit::None);
                }
                Action::ReplaceDirectory(path) => match env.validate_directory_target(&path) {
                    Ok(()) => {
                        drop(terminal);
                        return Ok(TuiExit::ReplaceSplit(path));
                    }
                    Err(error) => app.set_error(error.to_string()),
                },
                _ => {}
            },
            _ => continue,
        };
    }

    Ok(TuiExit::None)
}

fn launch_workspace_from_tui(
    terminal: &mut TerminalSession,
    env: &AppEnv,
    name: &str,
) -> Result<Option<String>> {
    let (frame, pending_warning) = match env.capture_frontmost_ghostty_window_frame() {
        Ok(frame) => (Some(frame), None),
        Err(error) => (
            None,
            Some(format!(
                "workspace launched without frame sync; gtab could not read the current Ghostty window frame: {error}"
            )),
        ),
    };

    terminal.suspend()?;

    let result = match frame {
        Some(frame) => env.launch_workspace_from_tui_with_frame(name, &frame),
        None => env.launch_workspace(name).map(|()| None),
    };

    match result {
        Ok(warning) => Ok(warning.or(pending_warning)),
        Err(error) => {
            terminal.resume()?;
            Err(error)
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    suspended: bool,
}

impl TerminalSession {
    fn start() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)
            .context("failed to enter alternate screen")?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))
            .context("failed to initialize terminal backend")?;
        Ok(Self {
            terminal,
            suspended: false,
        })
    }

    fn draw(&mut self, f: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(f).context("failed to draw frame")?;
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        // Drain any buffered terminal events (e.g. mouse button release) before
        // disabling raw mode. Without this, the release bytes can leak into the
        // shell's input buffer and appear as visible text (e.g. "0;9;4m").
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let _ = event::read();
        }

        disable_raw_mode().context("failed to disable raw mode")?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            Show
        )
        .context("failed to leave alternate screen")?;
        self.terminal.show_cursor().ok();
        self.suspended = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            Hide
        )
        .context("failed to re-enter alternate screen")?;
        enable_raw_mode().context("failed to re-enable raw mode")?;
        self.terminal.clear().ok();
        self.suspended = false;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.suspended {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            Show
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Dialog {
    None,
    SaveWorkspace,
    SaveDirectory,
    RenameWorkspace,
    RenameDirectory,
    ConfirmDeleteWorkspace,
    ConfirmDeleteDirectory,
    Settings,
    EditGhosttyShortcut,
    Help,
}

#[derive(Clone, Debug)]
enum StatusKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
struct StatusLine {
    kind: StatusKind,
    text: String,
}

#[derive(Clone, Debug)]
struct ClickState {
    index: usize,
    at: Instant,
}

#[derive(Clone, Debug)]
struct GridMetrics {
    columns: usize,
    rows_per_page: usize,
    column_widths: Vec<u16>,
}

impl GridMetrics {
    fn column_width(&self, column: usize) -> u16 {
        self.column_widths
            .get(column)
            .copied()
            .unwrap_or(GRID_CELL_MIN_WIDTH)
    }

    /// Column index under `offset_x` (relative to the grid origin), or `None`
    /// when the offset falls in the gap between two columns.
    fn column_at(&self, offset_x: u16) -> Option<usize> {
        let mut cursor = 0_u16;
        for column in 0..self.columns {
            let width = self.column_width(column);
            if offset_x < cursor.saturating_add(width) {
                return Some(column);
            }
            cursor = cursor.saturating_add(width).saturating_add(GRID_CELL_GAP);
            if offset_x < cursor {
                return None;
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
struct App {
    workspaces: Vec<Workspace>,
    directories: Vec<SavedDirectory>,
    mode: BrowserMode,
    selected: usize,
    list_offset: usize,
    list_area: Rect,
    /// Set while drawing: the visible list is laid out as a wrapping grid
    /// rather than one entry per row.
    grid_list: bool,
    shortcut_area: Rect,
    last_click: Option<ClickState>,
    filter: String,
    search_before_edit: Option<String>,
    dialog: Dialog,
    save_input: String,
    rename_input: String,
    rename_original: Option<String>,
    shortcut_input: String,
    shortcut_return_dialog: Dialog,
    status: Option<StatusLine>,
    status_expiry: Option<Instant>,
}

impl App {
    fn new(workspaces: Vec<Workspace>, directories: Vec<SavedDirectory>) -> Self {
        Self {
            workspaces,
            directories,
            mode: BrowserMode::Workspace,
            selected: 0,
            list_offset: 0,
            list_area: Rect::default(),
            grid_list: false,
            shortcut_area: Rect::default(),
            last_click: None,
            filter: String::new(),
            search_before_edit: None,
            dialog: Dialog::None,
            save_input: String::new(),
            rename_input: String::new(),
            rename_original: None,
            shortcut_input: String::new(),
            shortcut_return_dialog: Dialog::None,
            status: Some(StatusLine {
                kind: StatusKind::Info,
                text: "Enter launch  f directories  / filter  ? help".to_string(),
            }),
            status_expiry: None,
        }
    }

    fn reload_workspaces(&mut self, workspaces: Vec<Workspace>) {
        self.workspaces = workspaces;
        self.clear_pending_click();
        self.clamp_selection();
    }

    fn reload_directories(&mut self, directories: Vec<SavedDirectory>) {
        self.directories = directories;
        self.clear_pending_click();
        self.clamp_selection();
    }

    fn reset_dialogs(&mut self) {
        self.dialog = Dialog::None;
        self.save_input.clear();
        self.rename_input.clear();
        self.rename_original = None;
        self.shortcut_input.clear();
        self.shortcut_return_dialog = Dialog::None;
    }

    fn open_settings(&mut self, _env: &AppEnv) {
        self.dialog = Dialog::Settings;
    }

    fn open_shortcut_editor(&mut self, env: &AppEnv, return_dialog: Dialog) {
        self.shortcut_return_dialog = return_dialog;
        self.dialog = Dialog::EditGhosttyShortcut;
        self.shortcut_input = env.ghostty_shortcut_display().to_string();
    }

    fn open_rename_workspace(&mut self, name: String) {
        self.dialog = Dialog::RenameWorkspace;
        self.rename_input = name.clone();
        self.rename_original = Some(name);
    }

    fn open_rename_directory(&mut self, name: String) {
        self.dialog = Dialog::RenameDirectory;
        self.rename_input = name.clone();
        self.rename_original = Some(name);
    }

    fn switch_mode(&mut self, mode: BrowserMode) {
        self.mode = mode;
        self.filter.clear();
        self.search_before_edit = None;
        self.list_offset = 0;
        self.reset_visible_selection();
        match mode {
            BrowserMode::Workspace => self.set_info("Switched to workspace space"),
            BrowserMode::Directory => self.set_info("Switched to directory space"),
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return match self.mode {
                BrowserMode::Workspace => (0..self.workspaces.len()).collect(),
                BrowserMode::Directory => (0..self.directories.len()).collect(),
            };
        }

        let needle = self.filter.to_lowercase();
        match self.mode {
            BrowserMode::Workspace => self
                .workspaces
                .iter()
                .enumerate()
                .filter_map(|(index, workspace)| {
                    workspace
                        .name
                        .to_lowercase()
                        .contains(&needle)
                        .then_some(index)
                })
                .collect(),
            BrowserMode::Directory => self
                .directories
                .iter()
                .enumerate()
                .filter_map(|(index, directory)| {
                    directory
                        .name
                        .to_lowercase()
                        .contains(&needle)
                        .then_some(index)
                })
                .collect(),
        }
    }

    /// Grid labels for whichever space is showing.
    fn visible_labels(&self) -> Vec<String> {
        match self.mode {
            BrowserMode::Workspace => self
                .visible_workspaces()
                .iter()
                .map(|workspace| entry_label(&workspace.name))
                .collect(),
            BrowserMode::Directory => self
                .visible_directories()
                .iter()
                .map(|directory| entry_label(&directory.name))
                .collect(),
        }
    }

    fn visible_workspaces(&self) -> Vec<&Workspace> {
        if self.mode != BrowserMode::Workspace {
            return Vec::new();
        }

        self.visible_indices()
            .iter()
            .map(|index| &self.workspaces[*index])
            .collect()
    }

    fn visible_directories(&self) -> Vec<&SavedDirectory> {
        if self.mode != BrowserMode::Directory {
            return Vec::new();
        }

        self.visible_indices()
            .iter()
            .map(|index| &self.directories[*index])
            .collect()
    }

    fn selected_workspace(&self) -> Option<&Workspace> {
        if self.mode != BrowserMode::Workspace {
            return None;
        }

        let indices = self.visible_indices();
        indices
            .get(self.selected)
            .and_then(|index| self.workspaces.get(*index))
    }

    fn selected_directory(&self) -> Option<&SavedDirectory> {
        if self.mode != BrowserMode::Directory {
            return None;
        }

        let indices = self.visible_indices();
        indices
            .get(self.selected)
            .and_then(|index| self.directories.get(*index))
    }

    fn select_name(&mut self, name: &str) {
        let position = match self.mode {
            BrowserMode::Workspace => self
                .visible_workspaces()
                .iter()
                .position(|workspace| workspace.name == name),
            BrowserMode::Directory => self
                .visible_directories()
                .iter()
                .position(|directory| directory.name == name),
        };

        let Some(position) = position else {
            self.selected = 0;
            self.clear_pending_click();
            return;
        };

        self.selected = position;
        self.clear_pending_click();
    }

    fn no_selection_message(&self) -> &'static str {
        match self.mode {
            BrowserMode::Workspace => "No workspace selected",
            BrowserMode::Directory => "No directory selected",
        }
    }

    fn item_label_plural(&self) -> &'static str {
        match self.mode {
            BrowserMode::Workspace => "workspaces",
            BrowserMode::Directory => "directories",
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len.saturating_sub(1));
        }
    }

    fn reset_visible_selection(&mut self) {
        self.selected = 0;
        self.clear_pending_click();
        self.clamp_selection();
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
            self.clear_pending_click();
            return;
        }

        let max = len.saturating_sub(1) as isize;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
        self.clear_pending_click();
    }

    fn move_selection_wrapping(&mut self, delta: isize) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
            self.clear_pending_click();
            return;
        }

        let len_i = len as isize;
        let next = (self.selected as isize + delta).rem_euclid(len_i) as usize;
        self.selected = next;
        self.clear_pending_click();
    }

    fn move_to_start(&mut self) {
        self.selected = 0;
        self.clear_pending_click();
    }

    fn move_to_end(&mut self) {
        let len = self.visible_indices().len();
        if len > 0 {
            self.selected = len - 1;
        }
        self.clear_pending_click();
    }

    fn page_step(&self) -> isize {
        if !self.grid_list {
            return self.list_area.height.saturating_sub(1).max(5) as isize;
        }

        let metrics = self.grid_metrics();
        (metrics.columns.saturating_mul(metrics.rows_per_page.max(1))) as isize
    }

    fn move_vertical_selection(&mut self, direction: isize) {
        if !self.grid_list {
            self.move_selection(direction);
            return;
        }

        let metrics = self.grid_metrics();
        self.move_selection(direction.saturating_mul(metrics.columns.max(1) as isize));
    }

    fn ensure_grid_selection_visible(&mut self) {
        if !self.grid_list {
            return;
        }

        let len = self.visible_indices().len();
        if len == 0 {
            self.list_offset = 0;
            return;
        }

        let metrics = self.grid_metrics();
        let selected_row = self.selected / metrics.columns.max(1);
        let max_offset =
            total_grid_rows(len, metrics.columns).saturating_sub(metrics.rows_per_page);

        if selected_row < self.list_offset {
            self.list_offset = selected_row;
        } else if selected_row
            >= self
                .list_offset
                .saturating_add(metrics.rows_per_page.max(1))
        {
            self.list_offset = selected_row.saturating_sub(metrics.rows_per_page.max(1) - 1);
        }

        self.list_offset = self.list_offset.min(max_offset);
    }

    fn clamp_grid_offset(&mut self) {
        if !self.grid_list {
            return;
        }

        let len = self.visible_indices().len();
        let metrics = self.grid_metrics();
        let max_offset =
            total_grid_rows(len, metrics.columns).saturating_sub(metrics.rows_per_page);
        self.list_offset = self.list_offset.min(max_offset);
    }

    fn grid_metrics(&self) -> GridMetrics {
        let available_width = self.list_area.width.max(1);
        let rows_per_page = self.list_area.height.max(1) as usize;
        let labels: Vec<u16> = self
            .visible_labels()
            .iter()
            .map(|label| label.chars().count() as u16)
            .collect();

        // Start as wide as the grid may get, then shrink until the widest row
        // fits, so the names spread across the pane instead of bunching up on
        // the left. A pane too narrow for two columns simply scrolls.
        let mut columns = GRID_MAX_COLUMNS.min(labels.len()).max(1);
        while columns > 1 && grid_row_width(&labels, columns) > u32::from(available_width) {
            columns -= 1;
        }

        let column_widths = grid_column_widths(&labels, columns)
            .into_iter()
            .map(|width| width.min(available_width))
            .collect();

        GridMetrics {
            columns,
            rows_per_page,
            column_widths,
        }
    }

    fn clear_pending_click(&mut self) {
        self.last_click = None;
    }

    fn is_double_click(&self, index: usize, clicked_at: Instant) -> bool {
        self.last_click.as_ref().is_some_and(|last_click| {
            last_click.index == index
                && clicked_at.duration_since(last_click.at)
                    <= Duration::from_millis(DOUBLE_CLICK_MS)
        })
    }

    fn search_active(&self) -> bool {
        self.search_before_edit.is_some()
    }

    fn begin_search(&mut self, initial: Option<char>) {
        if self.search_before_edit.is_none() {
            self.search_before_edit = Some(self.filter.clone());
        }

        if let Some(ch) = initial {
            self.filter.push(ch);
            self.reset_visible_selection();
        }
    }

    fn commit_search(&mut self) {
        self.search_before_edit = None;
        self.clear_pending_click();
    }

    fn cancel_search(&mut self) {
        if let Some(previous) = self.search_before_edit.take() {
            self.filter = previous;
            self.reset_visible_selection();
        }
    }

    fn update_filter_after_edit(&mut self) {
        self.reset_visible_selection();
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status = Some(StatusLine {
            kind,
            text: text.into(),
        });
        self.status_expiry = Some(Instant::now() + Duration::from_secs(4));
    }

    fn set_success(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Success, text);
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Info, text);
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Error, text);
    }

    fn clear_status(&mut self) {
        self.status = None;
        self.status_expiry = None;
    }

    fn handle_key(&mut self, key: KeyEvent, env: &AppEnv) -> Result<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Action::Quit);
        }

        match self.dialog {
            Dialog::SaveWorkspace => self.handle_save_workspace_key(key),
            Dialog::SaveDirectory => self.handle_save_directory_key(key),
            Dialog::RenameWorkspace => self.handle_rename_workspace_key(key),
            Dialog::RenameDirectory => self.handle_rename_directory_key(key),
            Dialog::ConfirmDeleteWorkspace => self.handle_delete_workspace_key(key),
            Dialog::ConfirmDeleteDirectory => self.handle_delete_directory_key(key),
            Dialog::Settings => self.handle_settings_key(key, env),
            Dialog::EditGhosttyShortcut => self.handle_shortcut_key(key),
            Dialog::Help => self.handle_help_key(key),
            Dialog::None if self.search_active() => self.handle_search_key(key),
            Dialog::None => self.handle_main_key(key, env),
        }
    }

    fn handle_save_workspace_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter => {
                let name = self.save_input.trim().to_string();
                if name.is_empty() {
                    self.set_error("Workspace name cannot be empty");
                    return Ok(Action::None);
                }

                Ok(Action::SaveWorkspace(name))
            }
            KeyCode::Backspace => {
                self.save_input.pop();
                Ok(Action::None)
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.save_input.push(c);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_save_directory_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter => {
                let name = self.save_input.trim().to_string();
                if name.is_empty() {
                    self.set_error("Directory name cannot be empty");
                    return Ok(Action::None);
                }

                let current_dir =
                    env::current_dir().context("failed to resolve current directory")?;
                Ok(Action::SaveDirectory(name, current_dir))
            }
            KeyCode::Backspace => {
                self.save_input.pop();
                Ok(Action::None)
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.save_input.push(c);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_rename_workspace_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter => {
                let Some(original) = self.rename_original.clone() else {
                    self.reset_dialogs();
                    return Ok(Action::None);
                };

                let name = self.rename_input.trim().to_string();
                if name.is_empty() {
                    self.set_error("Workspace name cannot be empty");
                    return Ok(Action::None);
                }

                if name == original {
                    self.reset_dialogs();
                    self.set_info("Workspace name unchanged");
                    return Ok(Action::None);
                }

                Ok(Action::RenameWorkspace(original, name))
            }
            KeyCode::Backspace => {
                self.rename_input.pop();
                Ok(Action::None)
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.rename_input.push(c);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_rename_directory_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter => {
                let Some(original) = self.rename_original.clone() else {
                    self.reset_dialogs();
                    return Ok(Action::None);
                };

                let name = self.rename_input.trim().to_string();
                if name.is_empty() {
                    self.set_error("Directory name cannot be empty");
                    return Ok(Action::None);
                }

                if name == original {
                    self.reset_dialogs();
                    self.set_info("Directory name unchanged");
                    return Ok(Action::None);
                }

                Ok(Action::RenameDirectory(original, name))
            }
            KeyCode::Backspace => {
                self.rename_input.pop();
                Ok(Action::None)
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.rename_input.push(c);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_delete_workspace_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let Some(workspace) = self.selected_workspace() else {
                    self.reset_dialogs();
                    return Ok(Action::None);
                };

                Ok(Action::DeleteWorkspace(workspace.name.clone()))
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_delete_directory_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let Some(directory) = self.selected_directory() else {
                    self.reset_dialogs();
                    return Ok(Action::None);
                };

                Ok(Action::DeleteDirectory(directory.name.clone()))
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent, env: &AppEnv) -> Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.reset_dialogs();
                Ok(Action::None)
            }
            KeyCode::Char('c') | KeyCode::Char(' ') => Ok(Action::ToggleCloseTab),
            KeyCode::Char('g') => {
                self.open_shortcut_editor(env, Dialog::Settings);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_shortcut_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.dialog = self.shortcut_return_dialog.clone();
                self.shortcut_input.clear();
                return Ok(Action::None);
            }
            KeyCode::Enter => {
                let shortcut = self.shortcut_input.trim().to_string();
                if shortcut.is_empty() {
                    self.set_error("Ghostty shortcut cannot be empty");
                    return Ok(Action::None);
                }

                return Ok(Action::SetGhosttyShortcut(shortcut));
            }
            KeyCode::Backspace => {
                self.shortcut_input.pop();
                return Ok(Action::None);
            }
            _ => {}
        }

        if let Some(shortcut) = shortcut_string_for_key_event(key) {
            self.shortcut_input = shortcut;
            return Ok(Action::None);
        }

        match key.code {
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.shortcut_input.push(c);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.dialog = Dialog::None;
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<Action> {
        if key.code == KeyCode::Tab {
            self.move_selection_wrapping(1);
            return Ok(Action::None);
        }
        if key.code == KeyCode::BackTab {
            self.move_selection_wrapping(-1);
            return Ok(Action::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.code
        {
            match c.to_ascii_lowercase() {
                'n' | 'j' => {
                    self.move_selection_wrapping(1);
                    return Ok(Action::None);
                }
                'p' | 'k' => {
                    self.move_selection_wrapping(-1);
                    return Ok(Action::None);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.cancel_search();
                Ok(Action::None)
            }
            KeyCode::Enter => {
                self.commit_search();
                let total_items = match self.mode {
                    BrowserMode::Workspace => self.workspaces.len(),
                    BrowserMode::Directory => self.directories.len(),
                };
                self.set_info(format!(
                    "Showing {} of {} {}",
                    self.visible_indices().len(),
                    total_items,
                    self.item_label_plural()
                ));
                Ok(Action::None)
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.update_filter_after_edit();
                Ok(Action::None)
            }
            KeyCode::Up => {
                self.move_vertical_selection(-1);
                Ok(Action::None)
            }
            KeyCode::Down => {
                self.move_vertical_selection(1);
                Ok(Action::None)
            }
            KeyCode::PageUp => {
                self.move_selection(-self.page_step());
                Ok(Action::None)
            }
            KeyCode::PageDown => {
                self.move_selection(self.page_step());
                Ok(Action::None)
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.filter.push(c);
                self.update_filter_after_edit();
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent, env: &AppEnv) -> Result<Action> {
        match key.code {
            KeyCode::Char('q') => Ok(Action::Quit),
            KeyCode::Char('?') => {
                self.dialog = Dialog::Help;
                Ok(Action::None)
            }
            KeyCode::Char('/') => {
                self.begin_search(None);
                Ok(Action::None)
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.reset_visible_selection();
                    self.set_info(format!("Cleared {} filter", self.item_label_plural()));
                    return Ok(Action::None);
                }

                Ok(Action::Quit)
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('s') => {
                self.move_vertical_selection(1);
                Ok(Action::None)
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('w') => {
                self.move_vertical_selection(-1);
                Ok(Action::None)
            }
            KeyCode::Right if self.grid_list => {
                self.move_selection(1);
                Ok(Action::None)
            }
            KeyCode::Left if self.grid_list => {
                self.move_selection(-1);
                Ok(Action::None)
            }
            KeyCode::Home => {
                self.move_to_start();
                Ok(Action::None)
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.move_to_end();
                Ok(Action::None)
            }
            KeyCode::PageUp => {
                self.move_selection(-self.page_step());
                Ok(Action::None)
            }
            KeyCode::PageDown => {
                self.move_selection(self.page_step());
                Ok(Action::None)
            }
            KeyCode::Char('r') => Ok(Action::Refresh),
            KeyCode::Char('f') => {
                let next_mode = if self.mode == BrowserMode::Workspace {
                    BrowserMode::Directory
                } else {
                    BrowserMode::Workspace
                };
                self.switch_mode(next_mode);
                Ok(Action::None)
            }
            KeyCode::Enter => match self.mode {
                BrowserMode::Workspace => {
                    let Some(workspace) = self.selected_workspace() else {
                        self.set_error(self.no_selection_message());
                        return Ok(Action::None);
                    };
                    Ok(Action::LaunchWorkspace(workspace.name.clone()))
                }
                BrowserMode::Directory => {
                    let Some(directory) = self.selected_directory() else {
                        self.set_error(self.no_selection_message());
                        return Ok(Action::None);
                    };
                    Ok(Action::ReplaceDirectory(directory.path.clone()))
                }
            },
            KeyCode::Char('a') => match self.mode {
                BrowserMode::Workspace => {
                    self.dialog = Dialog::SaveWorkspace;
                    self.save_input.clear();
                    Ok(Action::None)
                }
                BrowserMode::Directory => {
                    self.dialog = Dialog::SaveDirectory;
                    self.save_input.clear();
                    Ok(Action::None)
                }
            },
            KeyCode::Char('e') if self.mode == BrowserMode::Workspace => {
                let Some(workspace) = self.selected_workspace() else {
                    self.set_error(self.no_selection_message());
                    return Ok(Action::None);
                };
                Ok(Action::EditWorkspace(workspace.name.clone()))
            }
            KeyCode::Char('n') => match self.mode {
                BrowserMode::Workspace => {
                    let Some(name) = self
                        .selected_workspace()
                        .map(|workspace| workspace.name.clone())
                    else {
                        self.set_error(self.no_selection_message());
                        return Ok(Action::None);
                    };
                    self.open_rename_workspace(name);
                    Ok(Action::None)
                }
                BrowserMode::Directory => {
                    let Some(name) = self
                        .selected_directory()
                        .map(|directory| directory.name.clone())
                    else {
                        self.set_error(self.no_selection_message());
                        return Ok(Action::None);
                    };
                    self.open_rename_directory(name);
                    Ok(Action::None)
                }
            },
            KeyCode::Char('d') => {
                let has_selection = match self.mode {
                    BrowserMode::Workspace => self.selected_workspace().is_some(),
                    BrowserMode::Directory => self.selected_directory().is_some(),
                };

                if has_selection {
                    self.dialog = match self.mode {
                        BrowserMode::Workspace => Dialog::ConfirmDeleteWorkspace,
                        BrowserMode::Directory => Dialog::ConfirmDeleteDirectory,
                    };
                } else {
                    self.set_error(self.no_selection_message());
                }
                Ok(Action::None)
            }
            KeyCode::Char('g') if self.mode == BrowserMode::Workspace => {
                self.open_shortcut_editor(env, Dialog::None);
                Ok(Action::None)
            }
            KeyCode::Char('t') if self.mode == BrowserMode::Workspace => {
                self.open_settings(env);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, env: &AppEnv) -> Result<Action> {
        if !matches!(self.dialog, Dialog::None) {
            return Ok(Action::None);
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.mode == BrowserMode::Workspace
                    && self.shortcut_contains(mouse.column, mouse.row)
                {
                    self.clear_pending_click();
                    self.open_shortcut_editor(env, Dialog::None);
                    return Ok(Action::None);
                }

                let Some(index) = self.list_index_at(mouse.column, mouse.row) else {
                    self.clear_pending_click();
                    return Ok(Action::None);
                };

                self.selected = index;
                let clicked_at = Instant::now();
                if self.is_double_click(index, clicked_at) {
                    self.clear_pending_click();
                    return Ok(match self.mode {
                        BrowserMode::Workspace => {
                            let Some(workspace) = self.selected_workspace() else {
                                return Ok(Action::None);
                            };
                            Action::LaunchWorkspace(workspace.name.clone())
                        }
                        BrowserMode::Directory => {
                            let Some(directory) = self.selected_directory() else {
                                return Ok(Action::None);
                            };
                            Action::ReplaceDirectory(directory.path.clone())
                        }
                    });
                }

                self.last_click = Some(ClickState {
                    index,
                    at: clicked_at,
                });
                let selected_name = match self.mode {
                    BrowserMode::Workspace => self.selected_workspace().map(|w| w.name.clone()),
                    BrowserMode::Directory => self.selected_directory().map(|d| d.name.clone()),
                };

                if let Some(name) = selected_name {
                    self.set_info(format!("Selected \"{name}\""));
                }
                Ok(Action::None)
            }
            MouseEventKind::ScrollDown if self.list_contains(mouse.column, mouse.row) => {
                self.move_vertical_selection(1);
                Ok(Action::None)
            }
            MouseEventKind::ScrollUp if self.list_contains(mouse.column, mouse.row) => {
                self.move_vertical_selection(-1);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn list_index_at(&self, column: u16, row: u16) -> Option<usize> {
        if !self.list_contains(column, row) {
            return None;
        }

        if self.grid_list {
            let metrics = self.grid_metrics();
            let relative_row = row.saturating_sub(self.list_area.y) as usize;
            let relative_x = column.saturating_sub(self.list_area.x);
            let column_index = metrics.column_at(relative_x)?;

            let index = (self.list_offset + relative_row)
                .saturating_mul(metrics.columns)
                .saturating_add(column_index);
            return (index < self.visible_indices().len()).then_some(index);
        }

        let relative_row = row.saturating_sub(self.list_area.y) as usize;
        let index = self.list_offset + relative_row;
        (index < self.visible_indices().len()).then_some(index)
    }

    fn list_contains(&self, column: u16, row: u16) -> bool {
        column >= self.list_area.x
            && column < self.list_area.x.saturating_add(self.list_area.width)
            && row >= self.list_area.y
            && row < self.list_area.y.saturating_add(self.list_area.height)
    }

    fn shortcut_contains(&self, column: u16, row: u16) -> bool {
        column >= self.shortcut_area.x
            && column
                < self
                    .shortcut_area
                    .x
                    .saturating_add(self.shortcut_area.width)
            && row >= self.shortcut_area.y
            && row
                < self
                    .shortcut_area
                    .y
                    .saturating_add(self.shortcut_area.height)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Refresh,
    LaunchWorkspace(String),
    SaveWorkspace(String),
    RenameWorkspace(String, String),
    EditWorkspace(String),
    DeleteWorkspace(String),
    ReplaceDirectory(PathBuf),
    SaveDirectory(String, PathBuf),
    RenameDirectory(String, String),
    DeleteDirectory(String),
    ToggleCloseTab,
    SetGhosttyShortcut(String),
}

#[derive(Clone, Copy)]
struct Theme {
    accent: Style,
    emphasis: Style,
    muted: Style,
    dim: Style,
    success: Style,
    error: Style,
    warning: Style,
    selection: Style,
    border: Style,
    border_active: Style,
    titlebar: Style,
    titlebar_dim: Style,
    section: Style,
}

impl Theme {
    fn detect() -> Self {
        if env::var_os("NO_COLOR").is_some() {
            return Self {
                accent: Style::default().add_modifier(Modifier::BOLD),
                emphasis: Style::default().add_modifier(Modifier::BOLD),
                muted: Style::default().add_modifier(Modifier::DIM),
                dim: Style::default().add_modifier(Modifier::DIM),
                success: Style::default().add_modifier(Modifier::BOLD),
                error: Style::default().add_modifier(Modifier::BOLD),
                warning: Style::default().add_modifier(Modifier::BOLD),
                selection: Style::default().add_modifier(Modifier::BOLD),
                border: Style::default(),
                border_active: Style::default().add_modifier(Modifier::BOLD),
                titlebar: Style::default().add_modifier(Modifier::BOLD),
                titlebar_dim: Style::default().add_modifier(Modifier::DIM),
                section: Style::default().add_modifier(Modifier::BOLD),
            };
        }

        Self {
            accent: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            // Use the terminal's default foreground (no absolute color) plus
            // weight so emphasized text stays readable on both dark and light
            // themes. Hardcoding Color::White made it invisible on light
            // backgrounds such as Ghostty's One Half Light (issue #6).
            emphasis: Style::default().add_modifier(Modifier::BOLD),
            muted: Style::default().fg(Color::Gray),
            dim: Style::default().fg(Color::DarkGray),
            success: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            error: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            warning: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            selection: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            border: Style::default().fg(Color::DarkGray),
            border_active: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            titlebar: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            titlebar_dim: Style::default().fg(Color::Black).bg(Color::Cyan),
            section: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App, env: &AppEnv) {
    let theme = Theme::detect();
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area, &theme);
        return;
    }

    let shell_area = shell_rect(area);
    let shell = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_active);
    let inner = shell.inner(shell_area);
    frame.render_widget(shell, shell_area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(3)])
        .split(inner);

    draw_body(frame, layout[0], app, env, &theme);
    draw_footer(frame, layout[1], app, &theme);

    match app.dialog {
        Dialog::None => {}
        Dialog::SaveWorkspace => draw_save_workspace_dialog(frame, app, &theme),
        Dialog::SaveDirectory => draw_save_directory_dialog(frame, app, &theme),
        Dialog::RenameWorkspace => draw_rename_workspace_dialog(frame, app, &theme),
        Dialog::RenameDirectory => draw_rename_directory_dialog(frame, app, &theme),
        Dialog::ConfirmDeleteWorkspace => draw_delete_workspace_dialog(frame, app, &theme),
        Dialog::ConfirmDeleteDirectory => draw_delete_directory_dialog(frame, app, &theme),
        Dialog::Settings => draw_settings_dialog(frame, app, env, &theme),
        Dialog::EditGhosttyShortcut => draw_shortcut_dialog(frame, app, env, &theme),
        Dialog::Help => draw_help_dialog(frame, &theme),
    }
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled("gtab", theme.accent),
            Span::raw(" needs more room"),
        ]),
        Line::default(),
        Line::from(format!("Current terminal: {}x{}", area.width, area.height)),
        Line::from(format!("Recommended minimum: {}x{}", MIN_WIDTH, MIN_HEIGHT)),
        Line::default(),
        Line::from("Resize the terminal to show the dialog layout."),
    ]);

    frame.render_widget(
        Paragraph::new(text).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_active)
                .title("Resize Required"),
        ),
        centered_rect(58, 40, area),
    );
}

/// How the workspace screen arranges its panels for the current width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceLayoutMode {
    /// list | layout preview | quick settings
    Columns,
    /// list | layout preview, quick settings hidden
    Split,
    /// layout preview stacked below a full-width list
    Stacked,
}

fn workspace_layout_mode(width: u16) -> WorkspaceLayoutMode {
    if width >= WORKSPACE_COLUMNS_MIN_WIDTH {
        WorkspaceLayoutMode::Columns
    } else if width >= MAIN_LIST_WIDTH + PREVIEW_MIN_WIDTH {
        WorkspaceLayoutMode::Split
    } else {
        WorkspaceLayoutMode::Stacked
    }
}

/// Rows the stacked layout preview needs, trimmed so the list keeps the larger
/// share of a short body.
fn stacked_preview_height(app: &App, body_height: u16) -> u16 {
    let Some(workspace) = app.selected_workspace() else {
        return 0;
    };

    let needed = if workspace.layout.is_empty() {
        if workspace.tabs.is_empty() {
            return 0;
        }
        // Legacy workspaces fall back to a single-line tab list.
        1
    } else {
        workspace
            .layout
            .iter()
            .map(|tab| {
                pane_grid_rows(&tab.root)
                    .saturating_mul(2)
                    .saturating_add(1)
                    .max(5)
                    .saturating_add(2)
            })
            .sum::<usize>()
    };

    // One extra row for the rule that separates the preview from the list.
    let needed = u16::try_from(needed.saturating_add(1)).unwrap_or(u16::MAX);
    let cap = body_height.saturating_sub(STACKED_LIST_MIN_ROWS);
    // Below the rule plus a tab header there is nothing worth showing.
    if cap < 2 {
        return 0;
    }
    // Too short for a pane map: keep the tab headers and give the rest back to
    // the list rather than reserving rows that stay blank.
    if cap < STACKED_PREVIEW_MAP_MIN_ROWS {
        return 2;
    }

    needed.min(cap)
}

/// Rows the stacked workspace grid actually fills, so the preview sits right
/// below the names instead of at the bottom of a mostly empty pane.
fn stacked_list_height(app: &mut App, inner: Rect, available: u16) -> u16 {
    let prompt = u16::from(app.search_active() || !app.filter.is_empty());
    // grid_metrics() reads list_area, so publish the grid's own rect first.
    app.list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(prompt),
        inner.width,
        available.saturating_sub(prompt),
    );

    let columns = app.grid_metrics().columns;
    let rows = total_grid_rows(app.visible_indices().len(), columns);
    let rows = u16::try_from(rows).unwrap_or(u16::MAX).max(1);

    rows.saturating_add(prompt).min(available)
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &mut App, env: &AppEnv, theme: &Theme) {
    let content = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border);
    let inner = content.inner(area);
    frame.render_widget(content, area);

    match app.mode {
        BrowserMode::Workspace => match workspace_layout_mode(inner.width) {
            WorkspaceLayoutMode::Stacked => {
                // Too narrow for side-by-side panels: drop quick settings and
                // give the layout preview the full width below the list.
                app.shortcut_area = Rect::default();
                app.grid_list = true;
                let preview_height = stacked_preview_height(app, inner.height);
                let available = inner.height.saturating_sub(preview_height);
                let list_height = stacked_list_height(app, inner, available);
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(list_height),
                        Constraint::Length(preview_height),
                        Constraint::Min(0),
                    ])
                    .split(inner);

                draw_workspace_list(frame, chunks[0], app, theme, Borders::NONE);
                if preview_height > 0 {
                    draw_workspace_tabs(frame, chunks[1], app, theme, Borders::TOP);
                }
            }
            mode => {
                app.grid_list = false;
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(MAIN_LIST_WIDTH),
                        Constraint::Min(PREVIEW_MIN_WIDTH),
                    ])
                    .split(inner);

                draw_workspace_list(frame, chunks[0], app, theme, Borders::RIGHT);
                if mode == WorkspaceLayoutMode::Columns {
                    draw_workspace_detail(frame, chunks[1], app, env, theme);
                } else {
                    app.shortcut_area = Rect::default();
                    draw_workspace_tabs(frame, chunks[1], app, theme, Borders::NONE);
                }
            }
        },
        BrowserMode::Directory => {
            app.shortcut_area = Rect::default();
            app.grid_list = true;
            draw_directory_list(frame, inner, app, theme);
        }
    }
}

fn draw_workspace_list(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    theme: &Theme,
    borders: Borders,
) {
    let panel = Block::default().borders(borders).border_style(theme.border);
    let inner = panel.inner(area);
    frame.render_widget(panel, area);

    // Only show the filter buffer when the user is in filter mode, or when a
    // filter is already applied.
    let show_prompt = app.search_active() || !app.filter.is_empty();
    let (prompt_area, list_area) = if show_prompt {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    };

    app.list_area = list_area;

    if let Some(prompt_area) = prompt_area {
        let width = prompt_area.width.max(1) as usize;
        let prefix = if width >= 2 { "/ " } else { "/" };
        let available = width.saturating_sub(prefix.chars().count());

        let mut raw = if app.filter.is_empty() {
            // Only show a hint when explicitly in filter mode.
            if app.search_active() {
                "type to filter".to_string()
            } else {
                String::new()
            }
        } else {
            app.filter.clone()
        };

        if available == 0 {
            raw.clear();
        }
        let shown = if available == 0 {
            String::new()
        } else {
            fit_text(&raw, available)
        };

        let value_style = if app.filter.is_empty() {
            theme.muted
        } else {
            theme.emphasis
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, theme.accent),
                Span::styled(shown, value_style),
            ])),
            prompt_area,
        );
    }

    if app.grid_list {
        // Stacked layout: the list is full width, so wrap names into columns
        // instead of burning a row on each one.
        draw_entry_grid(frame, list_area, app, theme);
        return;
    }

    let visible = app.visible_workspaces();
    let items: Vec<ListItem<'_>> = if visible.is_empty() {
        vec![ListItem::new(Line::from(vec![Span::styled(
            "no matches",
            theme.muted,
        )]))]
    } else {
        visible
            .iter()
            .map(|workspace| {
                ListItem::new(Span::styled(entry_label(&workspace.name), theme.emphasis))
            })
            .collect()
    };

    let mut state = ListState::default()
        .with_selected((!visible.is_empty()).then_some(app.selected))
        .with_offset(app.list_offset);

    let list = List::new(items).highlight_style(theme.selection);

    frame.render_stateful_widget(list, list_area, &mut state);
    app.list_offset = state.offset();
}

fn draw_directory_list(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: &Theme) {
    app.list_area = area;
    draw_entry_grid(frame, area, app, theme);
}

/// Wrapping grid of `[name]` cells, shared by directory space and the stacked
/// workspace list.
fn draw_entry_grid(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: &Theme) {
    app.clamp_grid_offset();
    app.ensure_grid_selection_visible();
    let labels = app.visible_labels();
    let text = if labels.is_empty() {
        Text::from(vec![Line::from(vec![Span::styled(
            "no matches",
            theme.muted,
        )])])
    } else {
        grid_text(app, &labels, theme)
    };

    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

fn grid_text(app: &App, labels: &[String], theme: &Theme) -> Text<'static> {
    let metrics = app.grid_metrics();
    let start_row = app.list_offset;
    let start_index = start_row.saturating_mul(metrics.columns);
    let end_index = labels
        .len()
        .min(start_index.saturating_add(metrics.columns.saturating_mul(metrics.rows_per_page)));
    let mut lines = Vec::new();

    for row_start in (start_index..end_index).step_by(metrics.columns.max(1)) {
        let row_end = end_index.min(row_start.saturating_add(metrics.columns));
        let mut spans = Vec::new();

        for index in row_start..row_end {
            let column = index - row_start;
            let label = fit_grid_cell(&labels[index], metrics.column_width(column));
            let style = if index == app.selected {
                theme.selection
            } else {
                theme.emphasis
            };
            spans.push(Span::styled(label, style));
            if index + 1 < row_end {
                spans.push(Span::raw(" ".repeat(GRID_CELL_GAP as usize)));
            }
        }

        lines.push(Line::from(spans));
    }

    Text::from(lines)
}

fn entry_label(name: &str) -> String {
    format!("[{name}]")
}

fn fit_grid_cell(label: &str, cell_width: u16) -> String {
    let width = cell_width as usize;
    let chars: Vec<char> = label.chars().collect();
    let truncated = if chars.len() > width {
        if width <= 3 {
            chars.into_iter().take(width).collect::<String>()
        } else {
            let mut text = chars
                .into_iter()
                .take(width.saturating_sub(3))
                .collect::<String>();
            text.push_str("...");
            text
        }
    } else {
        label.to_string()
    };

    format!("{truncated:<width$}")
}

/// Column count for a compact, balanced block: roughly square, widened only
/// when the entries would not otherwise fit on one page.
/// Width of one grid row: every column plus the gaps between them.
fn grid_row_width(labels: &[u16], columns: usize) -> u32 {
    let widths = grid_column_widths(labels, columns);
    let gaps = u32::from(GRID_CELL_GAP) * (widths.len() as u32 - 1);
    widths.iter().map(|width| u32::from(*width)).sum::<u32>() + gaps
}

/// Width of each column, sized to its own widest entry so short names do not
/// inherit the padding of the longest one in the grid.
fn grid_column_widths(labels: &[u16], columns: usize) -> Vec<u16> {
    let columns = columns.max(1);
    let mut widths = vec![GRID_CELL_MIN_WIDTH; columns];
    for (index, label) in labels.iter().enumerate() {
        let column = index % columns;
        widths[column] = widths[column].max(*label);
    }

    widths
        .into_iter()
        .map(|width| width.clamp(GRID_CELL_MIN_WIDTH, GRID_CELL_MAX_WIDTH))
        .collect()
}

fn total_grid_rows(len: usize, columns: usize) -> usize {
    if len == 0 {
        0
    } else {
        (len + columns.saturating_sub(1)) / columns.max(1)
    }
}

fn draw_workspace_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    env: &AppEnv,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    draw_workspace_tabs(frame, chunks[0], app, theme, Borders::RIGHT);
    draw_quick_settings(frame, chunks[1], app, env, theme);
}

fn draw_workspace_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
    borders: Borders,
) {
    let panel = Block::default().borders(borders).border_style(theme.border);
    let top_rule = u16::from(borders.contains(Borders::TOP));
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(top_rule),
        area.width.saturating_sub(2),
        area.height.saturating_sub(top_rule),
    );

    frame.render_widget(panel, area);
    frame.render_widget(
        Paragraph::new(workspace_tabs_preview_text(
            app,
            inner.width,
            inner.height,
            theme,
        )),
        inner,
    );
}

fn draw_quick_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    env: &AppEnv,
    theme: &Theme,
) {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    app.shortcut_area = Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1);

    frame.render_widget(
        Paragraph::new(quick_settings_text(app, env, inner.width, theme))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn workspace_tabs_text(app: &App, theme: &Theme) -> Text<'static> {
    let Some(workspace) = app.selected_workspace() else {
        return Text::default();
    };

    if workspace.tabs.is_empty() {
        return Text::default();
    }

    let mut spans = Vec::with_capacity(workspace.tabs.len().saturating_mul(2));
    for tab in &workspace.tabs {
        spans.push(Span::styled(format!("「{}」", tab.title), theme.accent));
        spans.push(Span::raw(" "));
    }

    Text::from(Line::from(spans))
}

fn workspace_tabs_preview_text(app: &App, width: u16, height: u16, theme: &Theme) -> Text<'static> {
    let Some(workspace) = app.selected_workspace() else {
        return Text::default();
    };

    if workspace.layout.is_empty() {
        if workspace.tabs.is_empty() {
            return Text::default();
        }
        // If we don't have layout data (legacy workspace), fall back to the
        // compact single-line tab list.
        return workspace_tabs_text(app, theme);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut remaining = height as usize;
    let map_width = (width as usize).max(12);

    for (index, tab) in workspace.layout.iter().enumerate() {
        if remaining == 0 {
            break;
        }

        // One line per pane row plus the dividers and the outer border, so a
        // three-row layout is not squeezed into a two-row box.
        let map_height = pane_grid_rows(&tab.root)
            .saturating_mul(2)
            .saturating_add(1)
            .max(5);

        let header = Line::from(vec![
            Span::styled(format!("{:>2}. ", index + 1), theme.dim),
            Span::styled(tab.title.clone(), theme.emphasis),
        ]);
        lines.push(header);
        remaining = remaining.saturating_sub(1);

        // Shrink rather than skip when the pane is short; a squeezed map falls
        // back to a pane count instead of vanishing.
        if remaining >= 3 {
            let rendered = render_tab_layout_ascii(tab, map_width, map_height.min(remaining));
            for row in rendered {
                if remaining == 0 {
                    break;
                }
                lines.push(Line::from(Span::styled(row, theme.dim)));
                remaining = remaining.saturating_sub(1);
            }
        }

        if remaining == 0 {
            break;
        }
        lines.push(Line::default());
        remaining = remaining.saturating_sub(1);
    }

    Text::from(lines)
}

fn render_tab_layout_ascii(tab: &WorkspaceTabLayout, width: usize, height: usize) -> Vec<String> {
    let width = width.max(8);
    let height = height.max(3);
    let mut buf = vec![vec![' '; width]; height];

    // Outer border.
    for x in 0..width {
        buf[0][x] = if x == 0 || x + 1 == width { '+' } else { '-' };
        buf[height - 1][x] = if x == 0 || x + 1 == width { '+' } else { '-' };
    }
    for y in 1..height.saturating_sub(1) {
        buf[y][0] = '|';
        buf[y][width - 1] = '|';
    }

    if width >= 3 && height >= 3 {
        let columns = pane_grid_columns(&tab.root);
        let rows = pane_grid_rows(&tab.root);
        match (
            pane_grid_edges(1, width - 2, columns),
            pane_grid_edges(1, height - 2, rows),
        ) {
            (Some(xs), Some(ys)) => {
                render_pane_layout_ascii(&tab.root, (0, columns), (0, rows), &xs, &ys, &mut buf)
            }
            // The map is too small to draw one cell per pane; summarise instead
            // of dropping panes silently.
            _ => {
                let summary = format!("{} panes", pane_leaf_count(&tab.root));
                draw_centered_label(&summary, 1, 1, width - 2, height - 2, &mut buf);
            }
        }
    }

    buf.into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect()
}

/// Number of grid columns a pane subtree spans. Sibling panes stacked
/// vertically share the same columns, so only horizontal splits add width.
fn pane_grid_columns(layout: &WorkspacePaneLayout) -> usize {
    match layout {
        WorkspacePaneLayout::Leaf { .. } => 1,
        WorkspacePaneLayout::SplitRight { left, right } => {
            pane_grid_columns(left) + pane_grid_columns(right)
        }
        WorkspacePaneLayout::SplitDown { top, bottom } => {
            pane_grid_columns(top).max(pane_grid_columns(bottom))
        }
    }
}

/// Number of grid rows a pane subtree spans.
fn pane_grid_rows(layout: &WorkspacePaneLayout) -> usize {
    match layout {
        WorkspacePaneLayout::Leaf { .. } => 1,
        WorkspacePaneLayout::SplitRight { left, right } => {
            pane_grid_rows(left).max(pane_grid_rows(right))
        }
        WorkspacePaneLayout::SplitDown { top, bottom } => {
            pane_grid_rows(top) + pane_grid_rows(bottom)
        }
    }
}

fn pane_leaf_count(layout: &WorkspacePaneLayout) -> usize {
    match layout {
        WorkspacePaneLayout::Leaf { .. } => 1,
        WorkspacePaneLayout::SplitRight { left, right } => {
            pane_leaf_count(left) + pane_leaf_count(right)
        }
        WorkspacePaneLayout::SplitDown { top, bottom } => {
            pane_leaf_count(top) + pane_leaf_count(bottom)
        }
    }
}

/// Track edges for `count` evenly sized cells separated by one-character
/// dividers. Cell `i` covers `edges[i]..edges[i + 1] - 1` and the divider that
/// follows it sits at `edges[i + 1] - 1`. Returns `None` when `size` cannot fit
/// one column per pane.
fn pane_grid_edges(start: usize, size: usize, count: usize) -> Option<Vec<usize>> {
    let count = count.max(1);
    let content = size.checked_sub(count - 1)?;
    if content < count {
        return None;
    }

    let base = content / count;
    let extra = content % count;
    let mut edges = Vec::with_capacity(count + 1);
    let mut cursor = start;
    edges.push(cursor);
    for index in 0..count {
        cursor += base + usize::from(index < extra) + 1;
        edges.push(cursor);
    }

    Some(edges)
}

fn render_pane_layout_ascii(
    layout: &WorkspacePaneLayout,
    columns: (usize, usize),
    rows: (usize, usize),
    xs: &[usize],
    ys: &[usize],
    buf: &mut [Vec<char>],
) {
    let (first_col, last_col) = columns;
    let (first_row, last_row) = rows;
    if last_col <= first_col || last_row <= first_row {
        return;
    }

    let x = xs[first_col];
    let y = ys[first_row];
    let w = xs[last_col].saturating_sub(1).saturating_sub(x);
    let h = ys[last_row].saturating_sub(1).saturating_sub(y);
    if w == 0 || h == 0 {
        return;
    }

    match layout {
        WorkspacePaneLayout::Leaf { working_dir } => {
            draw_centered_label(&working_dir_label(working_dir), x, y, w, h, buf);
        }
        WorkspacePaneLayout::SplitRight { left, right } => {
            if last_col - first_col < 2 {
                render_pane_layout_ascii(left, columns, rows, xs, ys, buf);
                return;
            }

            // Give each subtree the columns it spans so every pane ends up the
            // same width, instead of halving the remaining space at each level.
            let split = (first_col + pane_grid_columns(left)).clamp(first_col + 1, last_col - 1);
            render_pane_layout_ascii(left, (first_col, split), rows, xs, ys, buf);
            render_pane_layout_ascii(right, (split, last_col), rows, xs, ys, buf);
            draw_vertical_divider(xs[split].saturating_sub(1), y, h, buf);
        }
        WorkspacePaneLayout::SplitDown { top, bottom } => {
            if last_row - first_row < 2 {
                render_pane_layout_ascii(top, columns, rows, xs, ys, buf);
                return;
            }

            let split = (first_row + pane_grid_rows(top)).clamp(first_row + 1, last_row - 1);
            render_pane_layout_ascii(top, columns, (first_row, split), xs, ys, buf);
            render_pane_layout_ascii(bottom, columns, (split, last_row), xs, ys, buf);
            draw_horizontal_divider(ys[split].saturating_sub(1), x, w, buf);
        }
    }
}

fn draw_centered_label(label: &str, x: usize, y: usize, w: usize, h: usize, buf: &mut [Vec<char>]) {
    if w == 0 || h == 0 {
        return;
    }

    let row = y + h / 2;
    if row >= buf.len() {
        return;
    }

    // Keep a column of breathing room on each side so long names never run
    // into the pane dividers.
    let text_width = if w >= 5 { w - 2 } else { w };
    let fitted = fit_text(label, text_width);
    let start = x + w.saturating_sub(fitted.chars().count()) / 2;
    for (offset, ch) in fitted.chars().enumerate() {
        let column = start + offset;
        if column >= buf[row].len() {
            break;
        }
        buf[row][column] = ch;
    }
}

fn is_horizontal_rule(ch: char) -> bool {
    ch == '-' || ch == '+'
}

fn is_vertical_rule(ch: char) -> bool {
    ch == '|' || ch == '+'
}

fn buf_cell(buf: &[Vec<char>], x: usize, y: usize) -> Option<char> {
    buf.get(y).and_then(|row| row.get(x)).copied()
}

fn draw_vertical_divider(x: usize, y: usize, h: usize, buf: &mut [Vec<char>]) {
    for row in y..y.saturating_add(h) {
        if buf_cell(buf, x, row).is_none() {
            continue;
        }
        let crosses = x > 0 && buf_cell(buf, x - 1, row).is_some_and(is_horizontal_rule)
            || buf_cell(buf, x + 1, row).is_some_and(is_horizontal_rule);
        buf[row][x] = if crosses { '+' } else { '|' };
    }

    // Join the divider to whatever encloses it (outer border or parent rule).
    for row in [y.checked_sub(1), Some(y.saturating_add(h))]
        .into_iter()
        .flatten()
    {
        if buf_cell(buf, x, row).is_some_and(is_horizontal_rule) {
            buf[row][x] = '+';
        }
    }
}

fn draw_horizontal_divider(y: usize, x: usize, w: usize, buf: &mut [Vec<char>]) {
    for column in x..x.saturating_add(w) {
        if buf_cell(buf, column, y).is_none() {
            continue;
        }
        let crosses = y > 0 && buf_cell(buf, column, y - 1).is_some_and(is_vertical_rule)
            || buf_cell(buf, column, y + 1).is_some_and(is_vertical_rule);
        buf[y][column] = if crosses { '+' } else { '-' };
    }

    for column in [x.checked_sub(1), Some(x.saturating_add(w))]
        .into_iter()
        .flatten()
    {
        if buf_cell(buf, column, y).is_some_and(is_vertical_rule) {
            buf[y][column] = '+';
        }
    }
}

fn working_dir_label(working_dir: &str) -> String {
    let trimmed = working_dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(trimmed)
        .to_string()
}

fn quick_settings_text(_app: &App, env: &AppEnv, width: u16, theme: &Theme) -> Text<'static> {
    let shortcut = env.ghostty_shortcut_display().to_string();

    Text::from(vec![
        section_line(width, "Quick Settings", theme),
        joined_line(
            Rect::new(0, 0, width, 1),
            &format!("shortcut {shortcut}"),
            "click / g",
            theme.warning,
            theme.dim,
        ),
        Line::default(),
        section_line(width, "Status", theme),
        Line::from(vec![Span::styled("same-shell in Ghostty", theme.warning)]),
        Line::from("Ghostty-local only"),
        Line::default(),
        Line::from(vec![
            Span::styled("t", theme.accent),
            Span::raw(" full settings"),
        ]),
    ])
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let status = app
        .status
        .as_ref()
        .map(|status| {
            let (label, style) = match status.kind {
                StatusKind::Info => ("[i]", theme.muted),
                StatusKind::Success => ("[ok]", theme.success),
                StatusKind::Error => ("[!!]", theme.error),
            };

            Line::from(vec![
                Span::styled(format!("{label} "), style),
                Span::raw(status.text.clone()),
            ])
        })
        .unwrap_or_else(|| Line::from(vec![Span::styled("[ ] ready", theme.muted)]));

    let keys = if app.dialog == Dialog::Help {
        Line::from(vec![
            Span::styled("Esc", theme.accent),
            Span::raw(" close  "),
            Span::styled("q", theme.accent),
            Span::raw(" close"),
        ])
    } else if matches!(app.dialog, Dialog::SaveWorkspace | Dialog::SaveDirectory) {
        Line::from(vec![
            Span::styled("Enter", theme.accent),
            Span::raw(" save  "),
            Span::styled("Esc", theme.accent),
            Span::raw(" cancel"),
        ])
    } else if matches!(
        app.dialog,
        Dialog::RenameWorkspace | Dialog::RenameDirectory
    ) {
        Line::from(vec![
            Span::styled("Enter", theme.accent),
            Span::raw(" rename  "),
            Span::styled("Esc", theme.accent),
            Span::raw(" cancel"),
        ])
    } else if matches!(
        app.dialog,
        Dialog::ConfirmDeleteWorkspace | Dialog::ConfirmDeleteDirectory
    ) {
        Line::from(vec![
            Span::styled("y", theme.accent),
            Span::raw(" confirm  "),
            Span::styled("n", theme.accent),
            Span::raw(" cancel"),
        ])
    } else if matches!(app.dialog, Dialog::Settings) {
        Line::from(vec![
            Span::styled("c", theme.accent),
            Span::raw(" toggle  "),
            Span::styled("g", theme.accent),
            Span::raw(" ghostty shortcut  "),
            Span::styled("Esc", theme.accent),
            Span::raw(" close"),
        ])
    } else if matches!(app.dialog, Dialog::EditGhosttyShortcut) {
        Line::from(vec![
            Span::styled("Enter", theme.accent),
            Span::raw(" save  "),
            Span::styled("Esc", theme.accent),
            Span::raw(" back"),
        ])
    } else if app.search_active() {
        Line::from(vec![
            Span::styled("type", theme.accent),
            Span::raw(" filter  "),
            Span::styled("Enter", theme.accent),
            Span::raw(" keep  "),
            Span::styled("Esc", theme.accent),
            Span::raw(" revert"),
        ])
    } else if app.mode == BrowserMode::Workspace {
        Line::from(vec![
            Span::styled("Enter", theme.accent),
            Span::raw(" launch  "),
            Span::styled("f", theme.accent),
            Span::raw(" directories  "),
            Span::styled("/", theme.accent),
            Span::raw(" filter  "),
            Span::styled("a", theme.accent),
            Span::raw(" save  "),
            Span::styled("n", theme.accent),
            Span::raw(" rename  "),
            Span::styled("d", theme.accent),
            Span::raw(" remove  "),
            Span::styled("e", theme.accent),
            Span::raw(" edit  "),
            Span::styled("t", theme.accent),
            Span::raw(" settings  "),
            Span::styled("g", theme.accent),
            Span::raw(" shortcut  "),
            Span::styled("?", theme.accent),
            Span::raw(" help  "),
            Span::styled("q", theme.accent),
            Span::raw(" quit"),
        ])
    } else {
        Line::from(vec![
            Span::styled("Enter", theme.accent),
            Span::raw(" open  "),
            Span::styled("f", theme.accent),
            Span::raw(" workspaces  "),
            Span::styled("/", theme.accent),
            Span::raw(" filter  "),
            Span::styled("a", theme.accent),
            Span::raw(" save  "),
            Span::styled("n", theme.accent),
            Span::raw(" rename  "),
            Span::styled("d", theme.accent),
            Span::raw(" remove  "),
            Span::styled("?", theme.accent),
            Span::raw(" help  "),
            Span::styled("q", theme.accent),
            Span::raw(" quit"),
        ])
    };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let footer_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(Paragraph::new(status), footer_layout[0]);
    frame.render_widget(Paragraph::new(keys), footer_layout[1]);
}

fn draw_save_workspace_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let area = centered_rect(58, 34, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Save Workspace",
        "Enter save | Esc cancel",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Current Window", theme),
            Line::from("Save the frontmost Ghostty window as a workspace."),
            Line::default(),
            section_line(inner.width, "Name", theme),
            Line::from(Span::styled(
                if app.save_input.is_empty() {
                    "..."
                } else {
                    app.save_input.as_str()
                },
                theme.accent,
            )),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_save_directory_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let area = centered_rect(58, 34, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Save Directory",
        "Enter save | Esc cancel",
        theme,
    );
    let current_dir = env::current_dir()
        .ok()
        .map(|path| display_path(&path))
        .unwrap_or_else(|| "(unavailable)".to_string());
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Current Directory", theme),
            Line::from(current_dir),
            Line::default(),
            section_line(inner.width, "Name", theme),
            Line::from(Span::styled(
                if app.save_input.is_empty() {
                    "..."
                } else {
                    app.save_input.as_str()
                },
                theme.accent,
            )),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_rename_workspace_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let workspace_name = app.rename_original.as_deref().unwrap_or("this workspace");
    let area = centered_rect(58, 36, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Rename Workspace",
        "Enter rename | Esc cancel",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Selection", theme),
            Line::from(Span::styled(workspace_name, theme.emphasis)),
            Line::default(),
            section_line(inner.width, "New Name", theme),
            Line::from(Span::styled(
                if app.rename_input.is_empty() {
                    "..."
                } else {
                    app.rename_input.as_str()
                },
                theme.accent,
            )),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_rename_directory_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let directory_name = app.rename_original.as_deref().unwrap_or("this directory");
    let area = centered_rect(58, 36, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Rename Directory",
        "Enter rename | Esc cancel",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Selection", theme),
            Line::from(Span::styled(directory_name, theme.emphasis)),
            Line::default(),
            section_line(inner.width, "New Name", theme),
            Line::from(Span::styled(
                if app.rename_input.is_empty() {
                    "..."
                } else {
                    app.rename_input.as_str()
                },
                theme.accent,
            )),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_delete_workspace_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let workspace_name = app
        .selected_workspace()
        .map(|workspace| workspace.name.as_str())
        .unwrap_or("this workspace");

    let area = centered_rect(56, 34, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Delete Workspace",
        "y confirm | n cancel",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Selection", theme),
            Line::from(vec![
                Span::styled("Delete ", theme.error),
                Span::styled(format!("\"{workspace_name}\""), theme.emphasis),
                Span::raw("?"),
            ]),
            Line::default(),
            section_line(inner.width, "Effect", theme),
            Line::from("This removes the saved AppleScript file."),
            Line::from("The action cannot be undone from gtab."),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_delete_directory_dialog(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let directory_name = app
        .selected_directory()
        .map(|directory| directory.name.as_str())
        .unwrap_or("this directory");

    let area = centered_rect(56, 34, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Delete Directory",
        "y confirm | n cancel",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Selection", theme),
            Line::from(vec![
                Span::styled("Delete ", theme.error),
                Span::styled(format!("\"{directory_name}\""), theme.emphasis),
                Span::raw("?"),
            ]),
            Line::default(),
            section_line(inner.width, "Effect", theme),
            Line::from("This removes the saved directory entry."),
            Line::from("The action cannot be undone from gtab."),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_settings_dialog(frame: &mut Frame<'_>, _app: &App, env: &AppEnv, theme: &Theme) {
    let area = centered_rect(68, 52, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Settings",
        "c toggle | g ghostty shortcut | Esc close",
        theme,
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Workspace", theme),
            Line::from(vec![
                Span::styled("close_tab ", theme.dim),
                Span::styled(env.close_tab_display(), theme.warning),
            ]),
            Line::default(),
            section_line(inner.width, "Shortcut", theme),
            Line::from(vec![
                Span::styled("ghostty   ", theme.dim),
                Span::styled(env.ghostty_shortcut_display(), theme.warning),
            ]),
            Line::from(vec![
                Span::styled("mode      ", theme.dim),
                Span::styled(env.shortcut_mode_display(), theme.warning),
            ]),
            Line::from(vec![
                Span::styled("scope     ", theme.dim),
                Span::styled("Ghostty current shell only", theme.warning),
            ]),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_shortcut_dialog(frame: &mut Frame<'_>, app: &App, env: &AppEnv, theme: &Theme) {
    let area = centered_rect(62, 38, frame.area());
    let inner = draw_dialog_shell(
        frame,
        area,
        "Edit Ghostty Shortcut",
        "Enter save | Esc back",
        theme,
    );
    let current_input = if app.shortcut_input.is_empty() {
        env.ghostty_shortcut_display()
    } else {
        app.shortcut_input.as_str()
    };

    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Shortcut", theme),
            Line::from(vec![Span::styled(current_input, theme.accent)]),
            Line::default(),
            section_line(inner.width, "Input", theme),
            Line::from("Press the shortcut directly, or type it manually."),
            Line::from(shortcut_mode_hint(env)),
            Line::default(),
            section_line(inner.width, "Examples", theme),
            Line::from("cmd+g"),
            Line::from("cmd+shift+g"),
            Line::from("off"),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn shortcut_mode_hint(env: &AppEnv) -> &'static str {
    match env.shortcut_mode() {
        crate::core::ShortcutMode::Text => "This types `gtab` into the focused Ghostty shell.",
        crate::core::ShortcutMode::Shell => {
            "This sends a silent trigger handled by `gtab shell-init`."
        }
    }
}

fn draw_help_dialog(frame: &mut Frame<'_>, theme: &Theme) {
    let area = centered_rect(68, 58, frame.area());
    let inner = draw_dialog_shell(frame, area, "Help", "Esc close | q close", theme);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            section_line(inner.width, "Move", theme),
            Line::from("j / k / arrows  PgUp / PgDn  Home / End / G"),
            Line::default(),
            section_line(inner.width, "Search", theme),
            Line::from("/ starts filter"),
            Line::from("Tab / Shift-Tab  Ctrl-n/p  Ctrl-j/k move selection"),
            Line::from("Enter keep  Esc revert"),
            Line::default(),
            section_line(inner.width, "Actions", theme),
            Line::from("f toggle workspace/directory spaces"),
            Line::from("Workspace: Enter launch  a save  n rename  e edit  d remove"),
            Line::from("Directory: Enter replace split  a save  n rename  d remove"),
            Line::from("Workspace-only: g edit shortcut  t settings"),
            Line::from("r reload"),
            Line::from("q quit"),
            Line::default(),
            section_line(inner.width, "Layout", theme),
            Line::from("Workspace space: left list + tabs + quick settings."),
            Line::from("Directory space: adaptive multi-column directory grid."),
            Line::default(),
            section_line(inner.width, "Mouse", theme),
            Line::from("click select  double-click launch/replace"),
            Line::from("workspace: click shortcut to edit  wheel move"),
        ]))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_dialog_shell(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    footer: &str,
    theme: &Theme,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(joined_line(
            layout[0],
            title,
            "x",
            theme.titlebar,
            theme.titlebar_dim,
        ))
        .style(theme.titlebar),
        layout[0],
    );
    let footer_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(layout[2]);
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(theme.border),
        footer_layout[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(footer)).style(theme.dim),
        footer_layout[1],
    );

    layout[1]
}

fn shell_rect(area: Rect) -> Rect {
    let horizontal_margin = if area.width >= SHELL_MARGIN_MIN_WIDTH {
        2
    } else {
        0
    };
    let vertical_margin = if area.height >= SHELL_MARGIN_MIN_HEIGHT {
        1
    } else {
        0
    };

    Rect::new(
        area.x.saturating_add(horizontal_margin),
        area.y.saturating_add(vertical_margin),
        area.width
            .saturating_sub(horizontal_margin.saturating_mul(2)),
        area.height
            .saturating_sub(vertical_margin.saturating_mul(2)),
    )
}

fn joined_line(
    area: Rect,
    left: &str,
    right: &str,
    left_style: Style,
    right_style: Style,
) -> Line<'static> {
    let width = area.width.max(1) as usize;
    let mut left = left.to_string();
    let mut right = right.to_string();

    if left.chars().count() + right.chars().count() + 1 > width {
        let right_cap = (width / 2).max(12);
        right = fit_text(&right, right_cap.min(width.saturating_sub(1)));
        let remaining = width.saturating_sub(right.chars().count() + 1);
        left = fit_text(&left, remaining.max(1));
    }

    let left_width = left.chars().count();
    let right_width = right.chars().count();
    let gap = width.saturating_sub(left_width + right_width).max(1);

    Line::from(vec![
        Span::styled(left, left_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, right_style),
    ])
}

fn section_line(width: u16, label: &str, theme: &Theme) -> Line<'static> {
    let label = label.to_ascii_uppercase();
    let fill_width = (width as usize)
        .saturating_sub(label.chars().count() + 1)
        .clamp(6, 40);

    Line::from(vec![
        Span::styled(label, theme.section),
        Span::raw(" "),
        Span::styled("─".repeat(fill_width), theme.dim),
    ])
}

fn fit_text(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }

    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let keep = max_width - 3;
    let prefix: String = text.chars().take(keep).collect();
    format!("{prefix}...")
}

/// Centred popup sized as a share of `area`, but never squeezed below the
/// point where its labels stop being readable on a small terminal.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let width = scaled_dialog_side(area.width, percent_x, DIALOG_MIN_WIDTH);
    let height = scaled_dialog_side(area.height, percent_y, DIALOG_MIN_HEIGHT);

    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn scaled_dialog_side(available: u16, percent: u16, minimum: u16) -> u16 {
    let scaled = (u32::from(available) * u32::from(percent) / 100) as u16;
    scaled.clamp(minimum.min(available), available)
}

fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let home = env::var("HOME").ok();
    match home {
        Some(home) if raw == home => "~".to_string(),
        Some(home) if raw.starts_with(&(home.clone() + "/")) => raw.replacen(&home, "~", 1),
        _ => raw,
    }
}

fn is_text_input(modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    )
}

// (quick-search removed; filter mode starts explicitly with '/').

fn shortcut_string_for_key_event(key: KeyEvent) -> Option<String> {
    let captures_modified_key = key.modifiers.intersects(
        KeyModifiers::SUPER
            | KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    );
    let captures_named_key = !matches!(key.code, KeyCode::Char(_));
    if !captures_modified_key && !captures_named_key {
        return None;
    }

    let key_name = shortcut_key_name(key.code)?;
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("cmd".to_string());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    parts.push(key_name);
    Some(parts.join("+"))
}

fn shortcut_key_name(code: KeyCode) -> Option<String> {
    match code {
        KeyCode::Char(c) => shortcut_key_name_for_char(c).map(str::to_string),
        KeyCode::Enter => Some("enter".to_string()),
        KeyCode::Tab | KeyCode::BackTab => Some("tab".to_string()),
        KeyCode::Esc => Some("esc".to_string()),
        KeyCode::Backspace => Some("backspace".to_string()),
        KeyCode::Delete => Some("delete".to_string()),
        KeyCode::Left => Some("left".to_string()),
        KeyCode::Right => Some("right".to_string()),
        KeyCode::Up => Some("up".to_string()),
        KeyCode::Down => Some("down".to_string()),
        _ => None,
    }
}

fn shortcut_key_name_for_char(c: char) -> Option<&'static str> {
    Some(match c {
        'a'..='z' => match c {
            'a' => "a",
            'b' => "b",
            'c' => "c",
            'd' => "d",
            'e' => "e",
            'f' => "f",
            'g' => "g",
            'h' => "h",
            'i' => "i",
            'j' => "j",
            'k' => "k",
            'l' => "l",
            'm' => "m",
            'n' => "n",
            'o' => "o",
            'p' => "p",
            'q' => "q",
            'r' => "r",
            's' => "s",
            't' => "t",
            'u' => "u",
            'v' => "v",
            'w' => "w",
            'x' => "x",
            'y' => "y",
            'z' => "z",
            _ => unreachable!(),
        },
        'A'..='Z' => return shortcut_key_name_for_char(c.to_ascii_lowercase()),
        '0' => "0",
        '1' => "1",
        '2' => "2",
        '3' => "3",
        '4' => "4",
        '5' => "5",
        '6' => "6",
        '7' => "7",
        '8' => "8",
        '9' => "9",
        '`' | '~' => "`",
        '-' | '_' => "-",
        '=' | '+' => "=",
        '[' | '{' => "[",
        ']' | '}' => "]",
        '\\' | '|' => "\\",
        ';' | ':' => ";",
        '\'' | '"' => "'",
        ',' | '<' => ",",
        '.' | '>' => ".",
        '/' | '?' => "/",
        ' ' => "space",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Config, WorkspacePaneLayout, WorkspaceTab, WorkspaceTabLayout};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use std::path::PathBuf;

    fn workspace(name: &str) -> Workspace {
        Workspace {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{name}.applescript")),
            tabs: vec![WorkspaceTab {
                title: "tab".to_string(),
                working_dir: Some("/tmp/project".to_string()),
            }],
            layout: vec![],
        }
    }

    fn directory(name: &str, path: &str) -> SavedDirectory {
        SavedDirectory {
            name: name.to_string(),
            path: PathBuf::from(path),
        }
    }

    fn app(workspaces: Vec<Workspace>) -> App {
        App::new(workspaces, vec![])
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn env() -> AppEnv {
        AppEnv {
            base_dir: PathBuf::from("/tmp/gtab"),
            config_file: PathBuf::from("/tmp/gtab/config"),
            config: Config {
                close_tab: true,
                ghostty_shortcut: "cmd+g".to_string(),
                shortcut_mode: crate::core::ShortcutMode::Text,
            },
        }
    }

    fn text_lines(text: Text<'static>) -> Vec<String> {
        text.lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn single_click_selects_and_double_click_launches() {
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        app.list_area = Rect::new(0, 0, 40, 6);

        assert_eq!(
            app.handle_mouse(left_click(1, 1), &env()).unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 1);

        assert_eq!(
            app.handle_mouse(left_click(1, 1), &env()).unwrap(),
            Action::LaunchWorkspace("beta".to_string())
        );
    }

    #[test]
    fn emphasis_style_stays_visible_on_light_backgrounds() {
        // Regression for issue #6: emphasis previously hardcoded Color::White,
        // which is invisible on light terminal themes (e.g. One Half Light).
        // Emphasized text must rely on the terminal's default foreground plus
        // weight, not an absolute color, so it stays readable on any
        // background.
        let theme = Theme::detect();
        assert_eq!(theme.emphasis.fg, None);
        assert!(theme.emphasis.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn rendered_tui_uses_visible_foregrounds_on_light_terminals() {
        use ratatui::{Terminal, backend::TestBackend};

        // Render the real TUI and confirm (1) no cell forces an absolute white
        // foreground, which is invisible on light themes such as One Half Light
        // (issue #6), and (2) a non-selected workspace name renders with the
        // terminal's default foreground plus bold, so it stays readable on any
        // background.
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        let app_env = env();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, &app_env))
            .unwrap();
        let buffer = terminal.backend().buffer();

        for cell in buffer.content() {
            assert_ne!(
                cell.fg,
                Color::White,
                "rendered cell {:?} forces white fg",
                cell.symbol()
            );
        }

        // Locate "[beta]" by matching one cell per column (the UI contains
        // multi-byte border glyphs, so a byte offset is not a column index).
        let target: Vec<String> = "[beta]".chars().map(|c| c.to_string()).collect();
        let width = buffer.area.width;
        let height = buffer.area.height;
        let mut checked = false;
        for y in 0..height {
            let symbols: Vec<String> = (0..width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            for start in 0..symbols.len().saturating_sub(target.len()) + 1 {
                if symbols[start..start + target.len()] == target[..] {
                    for offset in 0..target.len() {
                        let cell = buffer.cell(((start + offset) as u16, y)).unwrap();
                        assert_eq!(cell.fg, Color::Reset, "name cell should use default fg");
                        assert!(
                            cell.modifier.contains(Modifier::BOLD),
                            "name cell should be bold"
                        );
                    }
                    checked = true;
                    break;
                }
            }
            if checked {
                break;
            }
        }
        assert!(checked, "expected [beta] in rendered output");
    }

    #[test]
    fn directory_grid_text_wraps_across_multiple_columns() {
        let theme = Theme::detect();
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![
            directory("docs", "/tmp/docs"),
            directory("circle", "/tmp/circle"),
            directory("notes", "/tmp/notes"),
        ];
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 20, 4);

        let text = grid_text(&app, &app.visible_labels(), &theme);
        let lines = text_lines(text);

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[docs]"));
        assert!(lines[0].contains("[circle]"));
        assert!(lines[1].contains("[notes]"));
    }

    #[test]
    fn directory_grid_click_maps_second_column_item() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![
            directory("docs", "/tmp/docs"),
            directory("circle", "/tmp/circle"),
            directory("notes", "/tmp/notes"),
        ];
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 20, 4);

        // Two columns: "[docs]"/"[notes]" (7 wide) then a 2 column gap.
        assert_eq!(app.list_index_at(0, 0), Some(0));
        assert_eq!(app.list_index_at(9, 0), Some(1));
        assert_eq!(app.list_index_at(7, 0), None);
    }

    #[test]
    fn directory_grid_down_moves_by_visual_row() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![
            directory("docs", "/tmp/docs"),
            directory("circle", "/tmp/circle"),
            directory("notes", "/tmp/notes"),
            directory("play", "/tmp/play"),
        ];
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 20, 4);

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Down), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn search_escape_restores_previous_filter() {
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        app.filter = "al".to_string();
        app.begin_search(Some('p'));

        assert_eq!(app.filter, "alp");
        app.cancel_search();
        assert_eq!(app.filter, "al");
        assert!(!app.search_active());
    }

    #[test]
    fn main_screen_typing_letter_does_not_start_filter_mode() {
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);

        assert!(app.filter.is_empty());
        assert!(!app.search_active());

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('x')), &env())
                .unwrap(),
            Action::None
        );
        assert!(app.filter.is_empty());
        assert!(!app.search_active());
    }

    #[test]
    fn filter_mode_navigation_wraps_with_tab_and_ctrl_n_p() {
        let mut app = app(vec![
            workspace("alpha"),
            workspace("beta"),
            workspace("gamma"),
        ]);
        app.begin_search(None);

        app.selected = 2;
        assert_eq!(
            app.handle_search_key(KeyEvent::from(KeyCode::Tab)).unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 0);

        app.selected = 0;
        assert_eq!(
            app.handle_search_key(KeyEvent::from(KeyCode::BackTab))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 2);

        app.selected = 2;
        assert_eq!(
            app.handle_search_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 0);

        app.selected = 0;
        assert_eq!(
            app.handle_search_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn filter_mode_navigation_wraps_with_ctrl_j_k() {
        let mut app = app(vec![
            workspace("alpha"),
            workspace("beta"),
            workspace("gamma"),
        ]);
        app.begin_search(None);

        app.selected = 2;
        assert_eq!(
            app.handle_search_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 0);

        app.selected = 0;
        assert_eq!(
            app.handle_search_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn quick_settings_show_shortcut_status() {
        let theme = Theme::detect();
        let app = app(vec![workspace("alpha")]);

        let lines = text_lines(quick_settings_text(&app, &env(), 28, &theme));

        assert!(lines.iter().any(|line| line.contains("shortcut cmd+g")));
        assert!(lines.iter().any(|line| line.contains("click / g")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("same-shell in Ghostty"))
        );
        assert!(lines.iter().any(|line| line.contains("Ghostty-local only")));
        assert!(!lines.iter().any(|line| line.contains("close_tab")));
    }

    #[test]
    fn main_screen_g_opens_shortcut_editor() {
        let mut app = app(vec![workspace("alpha")]);

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('g')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::EditGhosttyShortcut);
        assert_eq!(app.shortcut_return_dialog, Dialog::None);
        assert_eq!(app.shortcut_input, "cmd+g");
    }

    #[test]
    fn main_screen_n_opens_rename_dialog_with_existing_name() {
        let mut app = app(vec![workspace("alpha")]);

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('n')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::RenameWorkspace);
        assert_eq!(app.rename_original.as_deref(), Some("alpha"));
        assert_eq!(app.rename_input, "alpha");
    }

    #[test]
    fn rename_dialog_returns_rename_action() {
        let mut app = app(vec![workspace("alpha")]);
        app.open_rename_workspace("alpha".to_string());
        app.rename_input = "beta".to_string();

        assert_eq!(
            app.handle_rename_workspace_key(KeyEvent::from(KeyCode::Enter))
                .unwrap(),
            Action::RenameWorkspace("alpha".to_string(), "beta".to_string())
        );
    }

    #[test]
    fn rename_dialog_closes_without_action_when_name_is_unchanged() {
        let mut app = app(vec![workspace("alpha")]);
        app.open_rename_workspace("alpha".to_string());

        assert_eq!(
            app.handle_rename_workspace_key(KeyEvent::from(KeyCode::Enter))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::None);
        assert!(app.rename_original.is_none());
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Workspace name unchanged")
        );
    }

    #[test]
    fn save_dialog_rejects_empty_name() {
        let mut app = app(vec![workspace("alpha")]);
        app.dialog = Dialog::SaveWorkspace;
        app.save_input = "   ".to_string();

        assert_eq!(
            app.handle_save_workspace_key(KeyEvent::from(KeyCode::Enter))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::SaveWorkspace);
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Workspace name cannot be empty")
        );
    }

    #[test]
    fn main_screen_q_returns_quit_action() {
        let mut app = app(vec![workspace("alpha")]);

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('q')), &env())
                .unwrap(),
            Action::Quit
        );
    }

    #[test]
    fn main_screen_enter_without_selection_sets_error() {
        let mut app = app(vec![workspace("alpha")]);
        app.filter = "zzz".to_string();

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Enter), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("No workspace selected")
        );
    }

    #[test]
    fn main_screen_delete_without_selection_sets_error() {
        let mut app = app(vec![workspace("alpha")]);
        app.filter = "zzz".to_string();

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('d')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::None);
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("No workspace selected")
        );
    }

    #[test]
    fn main_screen_edit_without_selection_sets_error() {
        let mut app = app(vec![workspace("alpha")]);
        app.filter = "zzz".to_string();

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('e')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("No workspace selected")
        );
    }

    #[test]
    fn main_screen_escape_clears_filter_before_quitting() {
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        app.filter = "al".to_string();

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Esc), &env())
                .unwrap(),
            Action::None
        );
        assert!(app.filter.is_empty());
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Cleared workspaces filter")
        );
    }

    #[test]
    fn shortcut_capture_formats_modified_char_keys() {
        let shortcut = shortcut_string_for_key_event(KeyEvent::new(
            KeyCode::Char('?'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        ));

        assert_eq!(shortcut.as_deref(), Some("cmd+shift+/"));
    }

    #[test]
    fn shortcut_capture_formats_named_keys() {
        let shortcut =
            shortcut_string_for_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));

        assert_eq!(shortcut.as_deref(), Some("cmd+left"));
    }

    #[test]
    fn clicking_shortcut_opens_shortcut_editor() {
        let mut app = app(vec![workspace("alpha")]);
        app.shortcut_area = Rect::new(30, 2, 20, 1);

        assert_eq!(
            app.handle_mouse(left_click(31, 2), &env()).unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::EditGhosttyShortcut);
        assert_eq!(app.shortcut_return_dialog, Dialog::None);
        assert_eq!(app.shortcut_input, "cmd+g");
    }

    #[test]
    fn shortcut_dialog_records_modified_keys() {
        let mut app = app(vec![workspace("alpha")]);

        assert_eq!(
            app.handle_shortcut_key(KeyEvent::new(
                KeyCode::Char('G'),
                KeyModifiers::SUPER | KeyModifiers::SHIFT,
            ))
            .unwrap(),
            Action::None
        );
        assert_eq!(app.shortcut_input, "cmd+shift+g");
    }

    #[test]
    fn settings_shortcut_escape_returns_to_settings_dialog() {
        let mut app = app(vec![workspace("alpha")]);
        app.open_shortcut_editor(&env(), Dialog::Settings);

        assert_eq!(
            app.handle_shortcut_key(KeyEvent::from(KeyCode::Esc))
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::Settings);
        assert!(app.shortcut_input.is_empty());
    }

    #[test]
    fn settings_dialog_space_toggles_close_tab() {
        let mut app = app(vec![workspace("alpha")]);
        app.dialog = Dialog::Settings;

        assert_eq!(
            app.handle_settings_key(KeyEvent::from(KeyCode::Char(' ')), &env())
                .unwrap(),
            Action::ToggleCloseTab
        );
    }

    #[test]
    fn search_enter_commits_filter_and_sets_status() {
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        app.begin_search(Some('l'));

        assert_eq!(
            app.handle_search_key(KeyEvent::from(KeyCode::Enter))
                .unwrap(),
            Action::None
        );
        assert!(!app.search_active());
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Showing 1 of 2 workspaces")
        );
    }

    #[test]
    fn workspace_tabs_follow_applescript_order() {
        let theme = Theme::detect();
        let app = app(vec![Workspace {
            name: "alpha".to_string(),
            path: PathBuf::from("/tmp/alpha.applescript"),
            tabs: vec![
                WorkspaceTab {
                    title: "api".to_string(),
                    working_dir: Some("/tmp/project/api".to_string()),
                },
                WorkspaceTab {
                    title: "worker".to_string(),
                    working_dir: Some("/tmp/project/worker".to_string()),
                },
            ],
            layout: vec![],
        }]);

        let lines = text_lines(workspace_tabs_text(&app, &theme));

        assert_eq!(lines, vec!["「api」 「worker」 ".to_string()]);
    }

    #[test]
    fn workspace_tabs_are_empty_without_visible_selection() {
        let theme = Theme::detect();
        let mut app = app(vec![workspace("alpha"), workspace("beta")]);
        app.filter = "zzz".to_string();

        let lines = text_lines(workspace_tabs_text(&app, &theme));

        assert!(lines.is_empty());
    }

    #[test]
    fn workspace_tabs_are_empty_when_workspace_has_no_tabs() {
        let theme = Theme::detect();
        let app = app(vec![Workspace {
            name: "empty".to_string(),
            path: PathBuf::from("/tmp/empty.applescript"),
            tabs: vec![],
            layout: vec![],
        }]);

        let lines = text_lines(workspace_tabs_text(&app, &theme));

        assert!(lines.is_empty());
    }

    #[test]
    fn main_screen_f_toggles_directory_mode() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![directory("docs", "/tmp")];

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('f')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.mode, BrowserMode::Directory);
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Switched to directory space")
        );
    }

    #[test]
    fn directory_mode_enter_returns_replace_directory_action() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![directory("docs", "/tmp")];
        app.mode = BrowserMode::Directory;

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Enter), &env())
                .unwrap(),
            Action::ReplaceDirectory(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn directory_mode_a_opens_save_directory_dialog() {
        let mut app = app(vec![workspace("alpha")]);
        app.mode = BrowserMode::Directory;

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('a')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::SaveDirectory);
    }

    #[test]
    fn directory_mode_double_click_replaces_directory() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![directory("docs", "/tmp/docs"), directory("tmp", "/tmp")];
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 40, 6);

        // "[docs]" fills the first six columns, then a two column gap.
        assert_eq!(
            app.handle_mouse(left_click(8, 0), &env()).unwrap(),
            Action::None
        );
        assert_eq!(
            app.handle_mouse(left_click(8, 0), &env()).unwrap(),
            Action::ReplaceDirectory(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn directory_mode_n_opens_directory_rename_dialog() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = vec![directory("docs", "/tmp/docs")];
        app.mode = BrowserMode::Directory;

        assert_eq!(
            app.handle_main_key(KeyEvent::from(KeyCode::Char('n')), &env())
                .unwrap(),
            Action::None
        );
        assert_eq!(app.dialog, Dialog::RenameDirectory);
        assert_eq!(app.rename_original.as_deref(), Some("docs"));
    }

    fn leaf(working_dir: &str) -> WorkspacePaneLayout {
        WorkspacePaneLayout::Leaf {
            working_dir: working_dir.to_string(),
        }
    }

    fn split_right(left: WorkspacePaneLayout, right: WorkspacePaneLayout) -> WorkspacePaneLayout {
        WorkspacePaneLayout::SplitRight {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn split_down(top: WorkspacePaneLayout, bottom: WorkspacePaneLayout) -> WorkspacePaneLayout {
        WorkspacePaneLayout::SplitDown {
            top: Box::new(top),
            bottom: Box::new(bottom),
        }
    }

    fn tab_layout(root: WorkspacePaneLayout) -> WorkspaceTabLayout {
        WorkspaceTabLayout {
            title: "operation".to_string(),
            root,
        }
    }

    /// Widths of the cells between the vertical rules on a rendered row.
    fn column_widths(row: &str) -> Vec<usize> {
        row.trim_matches(|c| c == '+' || c == '|')
            .split(['|', '+'])
            .map(|cell| cell.chars().count())
            .collect()
    }

    #[test]
    fn pane_preview_gives_every_column_the_same_width() {
        // Regression: nested right splits used to halve the remaining space at
        // each level, so three panes rendered as 50% / 25% / 25%.
        let tab = tab_layout(split_right(
            leaf("/x/one"),
            split_right(leaf("/x/two"), leaf("/x/three")),
        ));

        let rendered = render_tab_layout_ascii(&tab, 56, 5);
        let widths = column_widths(&rendered[2]);

        assert_eq!(widths.len(), 3, "expected three columns in {:?}", rendered);
        let min = widths.iter().min().copied().unwrap();
        let max = widths.iter().max().copied().unwrap();
        assert!(
            max - min <= 1,
            "columns should be evenly distributed, got {widths:?}"
        );
    }

    #[test]
    fn pane_preview_keeps_a_stacked_column_half_width() {
        // A pane split vertically still occupies a single grid column, so the
        // map stays 50/50 instead of shrinking the stacked side.
        let tab = tab_layout(split_right(
            leaf("/x/one"),
            split_down(leaf("/x/two"), leaf("/x/three")),
        ));

        let rendered = render_tab_layout_ascii(&tab, 56, 5);
        let widths = column_widths(&rendered[1]);

        assert_eq!(widths.len(), 2, "expected two columns in {:?}", rendered);
        assert!(
            widths[0].abs_diff(widths[1]) <= 1,
            "stacked column should keep half the width, got {widths:?}"
        );
    }

    #[test]
    fn pane_preview_renders_every_row_of_a_three_way_stack() {
        // Regression: the map was a fixed five lines tall, so the third pane of
        // a vertical stack was dropped from the preview entirely.
        let tab = tab_layout(split_down(
            leaf("/x/one"),
            split_down(leaf("/x/two"), leaf("/x/three")),
        ));

        let height = pane_grid_rows(&tab.root) * 2 + 1;
        let rendered = render_tab_layout_ascii(&tab, 40, height);
        let joined = rendered.join("\n");

        for name in ["one", "two", "three"] {
            assert!(joined.contains(name), "missing {name} in\n{joined}");
        }
    }

    #[test]
    fn pane_preview_height_follows_the_row_count() {
        assert_eq!(pane_grid_rows(&leaf("/x/one")), 1);
        assert_eq!(
            pane_grid_rows(&split_right(leaf("/x/one"), leaf("/x/two"))),
            1
        );
        assert_eq!(
            pane_grid_rows(&split_down(
                leaf("/x/one"),
                split_down(leaf("/x/two"), leaf("/x/three"))
            )),
            3
        );
        assert_eq!(
            pane_grid_columns(&split_right(
                leaf("/x/one"),
                split_down(leaf("/x/two"), leaf("/x/three"))
            )),
            2
        );
    }

    #[test]
    fn pane_preview_labels_keep_clear_of_the_dividers() {
        let tab = tab_layout(split_right(
            leaf("/x/a-very-long-directory-name"),
            leaf("/x/two"),
        ));

        let rendered = render_tab_layout_ascii(&tab, 40, 5);
        for row in &rendered[1..rendered.len() - 1] {
            let chars: Vec<char> = row.chars().collect();
            for (index, ch) in chars.iter().enumerate() {
                if *ch != '|' {
                    continue;
                }
                if let Some(next) = chars.get(index + 1) {
                    assert!(
                        *next == ' ' || *next == '|',
                        "label touches a divider in {row:?}"
                    );
                }
                if index > 0 {
                    let previous = chars[index - 1];
                    assert!(
                        previous == ' ' || previous == '|',
                        "label touches a divider in {row:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pane_preview_summarises_when_the_map_is_too_narrow() {
        let mut root = leaf("/x/0");
        for index in 1..12 {
            root = split_right(root, leaf(&format!("/x/{index}")));
        }

        let rendered = render_tab_layout_ascii(&tab_layout(root), 14, 5);
        assert!(
            rendered.join("\n").contains("12 panes"),
            "expected a pane count fallback in\n{}",
            rendered.join("\n")
        );
    }

    fn mixed_length_directories() -> Vec<SavedDirectory> {
        [
            "blog",
            "design-limns",
            "gtab",
            "kage",
            "limns",
            "limns-admin",
            "md",
            "new",
            "paper",
            "reel",
            "tag-data",
            "video",
        ]
        .into_iter()
        .map(|name| directory(name, "/tmp"))
        .collect()
    }

    #[test]
    fn directory_grid_fills_the_available_width() {
        // Regression: the column count was ceil(sqrt(len)) and ignored the pane
        // width, so 12 names sat in a 4 column block with the right half of a
        // wide pane left empty.
        let mut app = app(vec![workspace("alpha")]);
        app.directories = mixed_length_directories();
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 73, 40);

        // A 7th column would need 74 columns of width, so 6 is the widest fit.
        let metrics = app.grid_metrics();
        assert_eq!(metrics.columns, 6);

        let theme = Theme::detect();
        let lines = text_lines(grid_text(&app, &app.visible_labels(), &theme));
        assert_eq!(lines.len(), 2, "expected a 6x2 block: {lines:?}");
        assert_eq!(lines[0].trim_end().chars().count(), 66, "{:?}", lines[0]);
        for line in &lines {
            assert!(
                line.chars().count() <= 73,
                "row overflows the pane: {line:?}"
            );
        }
    }

    #[test]
    fn directory_grid_sizes_each_column_independently() {
        // Regression: the grid used one global cell width, so every short name
        // inherited the padding of the longest entry in the whole grid.
        let mut app = app(vec![workspace("alpha")]);
        app.directories = mixed_length_directories();
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 73, 40);

        let theme = Theme::detect();
        let lines = text_lines(grid_text(&app, &app.visible_labels(), &theme));
        // Only the 2 column gap separates them; "[blog]" is not padded out to
        // the width of "[design-limns]".
        assert!(
            lines[0].starts_with("[blog]  [design-limns]"),
            "unexpected column widths: {:?}",
            lines[0]
        );
    }

    /// The cap keeps an ultra-wide pane from shredding the list into slivers.
    #[test]
    fn directory_grid_stops_at_the_column_cap() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = mixed_length_directories();
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 400, 40);

        assert_eq!(app.grid_metrics().columns, GRID_MAX_COLUMNS);
    }

    #[test]
    fn directory_grid_columns_stay_aligned_across_rows() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = ["a", "bbbbbbbbbbbb", "c", "d", "e", "f"]
            .into_iter()
            .map(|name| directory(name, "/tmp"))
            .collect();
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 40, 10);

        let theme = Theme::detect();
        let lines = text_lines(grid_text(&app, &app.visible_labels(), &theme));
        let offsets: Vec<Vec<usize>> = lines
            .iter()
            .map(|line| line.match_indices('[').map(|(index, _)| index).collect())
            .collect();

        assert!(offsets.len() > 1);
        // The last row is allowed to be short, but every cell it does draw must
        // start under the matching cell of the first row.
        for row in &offsets[1..] {
            assert_eq!(
                *row,
                offsets[0][..row.len()],
                "columns drifted between rows: {lines:?}"
            );
        }
    }

    #[test]
    fn directory_grid_adds_columns_when_a_page_cannot_hold_the_list() {
        let mut app = app(vec![workspace("alpha")]);
        app.directories = (0..40)
            .map(|index| directory(&format!("dir{index}"), "/tmp"))
            .collect();
        app.mode = BrowserMode::Directory;
        app.grid_list = true;
        app.list_area = Rect::new(0, 0, 120, 6);

        let metrics = app.grid_metrics();
        assert!(
            total_grid_rows(40, metrics.columns) <= metrics.rows_per_page,
            "expected the list to fit one page, got {} columns",
            metrics.columns
        );
    }

    fn workspace_with_layout(name: &str, root: WorkspacePaneLayout) -> Workspace {
        let mut workspace = workspace(name);
        workspace.layout = vec![tab_layout(root)];
        workspace
    }

    fn render_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let app_env = env();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app, &app_env)).unwrap();
        let buffer = terminal.backend().buffer();

        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn row_index(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    #[test]
    fn workspace_layout_mode_switches_on_width() {
        assert_eq!(
            workspace_layout_mode(WORKSPACE_COLUMNS_MIN_WIDTH),
            WorkspaceLayoutMode::Columns
        );
        assert_eq!(
            workspace_layout_mode(WORKSPACE_COLUMNS_MIN_WIDTH - 1),
            WorkspaceLayoutMode::Split
        );
        assert_eq!(
            workspace_layout_mode(MAIN_LIST_WIDTH + PREVIEW_MIN_WIDTH),
            WorkspaceLayoutMode::Split
        );
        assert_eq!(
            workspace_layout_mode(MAIN_LIST_WIDTH + PREVIEW_MIN_WIDTH - 1),
            WorkspaceLayoutMode::Stacked
        );
    }

    #[test]
    fn narrow_workspace_screen_stacks_the_preview_below_the_list() {
        // Regression: the three column layout was kept at every width, so at
        // the minimum terminal size the pane map was squeezed into ~10 columns.
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_right(
                leaf("/x/one"),
                split_right(leaf("/x/two"), leaf("/x/three")),
            ),
        )]);

        // 52 columns was the old hard floor and is still too narrow for the
        // side-by-side panels.
        let rows = render_rows(&mut app, 52, 18);
        let joined = rows.join("\n");

        assert!(
            !joined.contains("QUICK SETTINGS"),
            "quick settings should be hidden when narrow:\n{joined}"
        );

        let list_row = row_index(&rows, "[alpha]").expect("workspace list");
        let header_row = row_index(&rows, "1. operation").expect("preview header");
        assert!(
            header_row > list_row,
            "preview should sit below the list:\n{joined}"
        );

        // The map now gets the full body width instead of a squeezed column.
        let map_row = rows
            .iter()
            .find(|row| row.contains("one") && row.contains("three"))
            .expect("pane map");
        let widths = column_widths(map_row.trim().trim_matches('\u{2502}').trim());
        assert_eq!(widths.len(), 3, "expected three panes in {map_row:?}");
        assert!(
            widths.iter().all(|width| *width >= 10),
            "panes should be readable, got {widths:?}"
        );
    }

    #[test]
    fn medium_workspace_screen_keeps_the_preview_beside_the_list() {
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_right(leaf("/x/one"), leaf("/x/two")),
        )]);

        let width = MAIN_LIST_WIDTH + PREVIEW_MIN_WIDTH + 8;
        let rows = render_rows(&mut app, width, 18);
        let joined = rows.join("\n");

        assert!(
            !joined.contains("QUICK SETTINGS"),
            "quick settings should be hidden before the preview shrinks:\n{joined}"
        );
        assert_eq!(
            row_index(&rows, "[alpha]"),
            row_index(&rows, "1. operation"),
            "list and preview should share a row:\n{joined}"
        );
    }

    #[test]
    fn wide_workspace_screen_keeps_all_three_columns() {
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_right(leaf("/x/one"), leaf("/x/two")),
        )]);

        let rows = render_rows(&mut app, 120, 18);
        let joined = rows.join("\n");

        assert!(joined.contains("QUICK SETTINGS"), "{joined}");
        // The longest quick settings line must not wrap at this width.
        assert!(joined.contains("same-shell in Ghostty"), "{joined}");
        assert!(joined.contains("shortcut cmd+g"), "{joined}");
    }

    #[test]
    fn hiding_quick_settings_clears_its_click_target() {
        let mut app = app(vec![workspace_with_layout("alpha", leaf("/x/one"))]);

        let _ = render_rows(&mut app, 120, 18);
        assert_ne!(app.shortcut_area, Rect::default());

        let _ = render_rows(&mut app, 52, 18);
        assert_eq!(
            app.shortcut_area,
            Rect::default(),
            "a hidden shortcut row must not stay clickable"
        );
    }

    #[test]
    fn stacked_preview_leaves_the_list_usable() {
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_down(leaf("/x/one"), split_down(leaf("/x/two"), leaf("/x/three"))),
        )]);
        app.selected = 0;

        for body_height in 6..40_u16 {
            let preview = stacked_preview_height(&app, body_height);
            assert!(
                preview <= body_height.saturating_sub(STACKED_LIST_MIN_ROWS),
                "preview {preview} left too little room in {body_height} rows"
            );
        }
    }

    #[test]
    fn dialogs_stop_shrinking_before_they_become_unreadable() {
        let small = Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT);
        let dialog = centered_rect(58, 34, small);

        assert_eq!(dialog.width, DIALOG_MIN_WIDTH);
        assert!(dialog.width <= small.width);
        assert_eq!(dialog.height, DIALOG_MIN_HEIGHT);
        assert!(dialog.height <= small.height);
        assert!(dialog.right() <= small.right());
        assert!(dialog.bottom() <= small.bottom());
    }

    #[test]
    fn dialogs_never_exceed_a_terminal_narrower_than_the_minimum() {
        let tiny = Rect::new(0, 0, DIALOG_MIN_WIDTH - 10, DIALOG_MIN_HEIGHT - 3);
        let dialog = centered_rect(58, 34, tiny);

        assert_eq!(dialog.width, tiny.width);
        assert_eq!(dialog.height, tiny.height);
    }

    #[test]
    fn dialogs_keep_their_share_of_a_roomy_terminal() {
        let area = Rect::new(0, 0, 150, 40);
        let dialog = centered_rect(58, 34, area);

        assert_eq!(dialog.width, 87);
        assert_eq!(dialog.height, 13);
        assert_eq!(dialog.x, (150 - 87) / 2);
        assert_eq!(dialog.y, (40 - 13) / 2);
    }

    #[test]
    fn the_minimum_terminal_still_renders_list_preview_and_footer() {
        // The stacked layout works far below the old 52 column floor, so the
        // resize notice should not take over at sizes the UI can handle.
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_right(leaf("/x/one"), leaf("/x/two")),
        )]);

        let rows = render_rows(&mut app, MIN_WIDTH, MIN_HEIGHT);
        let joined = rows.join("\n");

        assert!(!joined.contains("needs more room"), "{joined}");
        assert!(joined.contains("[alpha]"), "{joined}");
        assert!(joined.contains("1. operation"), "{joined}");
        // The footer keeps its status line and key hints.
        assert!(joined.contains("[i]"), "{joined}");
        assert!(joined.contains("Enter launch"), "{joined}");
    }

    #[test]
    fn terminals_below_the_minimum_still_get_the_resize_notice() {
        let mut app = app(vec![workspace("alpha")]);

        let rows = render_rows(&mut app, MIN_WIDTH - 1, MIN_HEIGHT);
        assert!(rows.join("\n").contains("needs more room"));
    }

    fn grid_workspaces() -> Vec<Workspace> {
        ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
            .into_iter()
            .map(workspace)
            .collect()
    }

    #[test]
    fn stacked_workspace_list_wraps_names_into_columns() {
        let mut app = app(grid_workspaces());

        let rows = render_rows(&mut app, 49, 24);
        let joined = rows.join("\n");
        let first = rows
            .iter()
            .find(|row| row.contains("[alpha]"))
            .expect("workspace names");

        assert!(
            first.contains("[beta]"),
            "names should share a row when stacked:\n{joined}"
        );
        assert!(app.grid_list);
    }

    #[test]
    fn wide_workspace_list_keeps_one_name_per_row() {
        let mut app = app(grid_workspaces());

        let rows = render_rows(&mut app, 120, 24);
        let first = rows
            .iter()
            .find(|row| row.contains("[alpha]"))
            .expect("workspace names");

        assert!(
            !first.contains("[beta]"),
            "the 24 column list should stay vertical: {first:?}"
        );
        assert!(!app.grid_list);
    }

    #[test]
    fn stacked_workspace_grid_moves_by_column_and_by_cell() {
        let mut app = app(grid_workspaces());
        let _ = render_rows(&mut app, 49, 24);

        let columns = app.grid_metrics().columns;
        assert!(columns > 1, "expected a multi column grid");

        app.selected = 0;
        app.handle_main_key(KeyEvent::from(KeyCode::Down), &env())
            .unwrap();
        assert_eq!(app.selected, columns, "Down should move a whole row");

        app.selected = 0;
        app.handle_main_key(KeyEvent::from(KeyCode::Right), &env())
            .unwrap();
        assert_eq!(app.selected, 1, "Right should move one cell");

        app.handle_main_key(KeyEvent::from(KeyCode::Left), &env())
            .unwrap();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn stacked_workspace_grid_maps_clicks_to_the_right_cell() {
        let mut app = app(grid_workspaces());
        let _ = render_rows(&mut app, 49, 24);

        let metrics = app.grid_metrics();
        let origin = app.list_area.x;
        let second_column = origin + metrics.column_width(0) + GRID_CELL_GAP;

        assert_eq!(app.list_index_at(origin, app.list_area.y), Some(0));
        assert_eq!(app.list_index_at(second_column, app.list_area.y), Some(1));
        assert_eq!(
            app.list_index_at(origin, app.list_area.y + 1),
            Some(metrics.columns)
        );
    }

    #[test]
    fn stacked_list_sits_directly_above_the_preview() {
        // Regression: the preview was pinned to the bottom of the body, so a
        // short grid left a large hole in the middle of the screen.
        let mut app = app(vec![workspace_with_layout(
            "alpha",
            split_right(leaf("/x/one"), leaf("/x/two")),
        )]);

        let rows = render_rows(&mut app, 49, 30);
        let joined = rows.join("\n");
        let header = row_index(&rows, "1. operation").expect("preview header");

        assert!(header >= 2, "{joined}");
        assert!(
            rows[header - 2].contains("[alpha]"),
            "the rule should follow the last name row:\n{joined}"
        );
    }

    #[test]
    fn stacked_grid_column_count_is_stable_across_frames() {
        // grid_metrics() reads list_area, which the stacked layout sizes from
        // grid_metrics(); the feedback loop must settle immediately.
        let mut app = app(grid_workspaces());

        let _ = render_rows(&mut app, 49, 24);
        let first = app.grid_metrics().columns;
        let first_area = app.list_area;

        let _ = render_rows(&mut app, 49, 24);
        assert_eq!(app.grid_metrics().columns, first);
        assert_eq!(app.list_area, first_area);
    }

    #[test]
    fn the_shell_margin_only_appears_when_columns_are_to_spare() {
        // Regression: the margin threshold used to be MIN_WIDTH + 10, so
        // lowering MIN_WIDTH quietly started spending four columns on
        // decoration exactly where the stacked layout needs them.
        let narrow = shell_rect(Rect::new(0, 0, SHELL_MARGIN_MIN_WIDTH - 1, 30));
        assert_eq!(narrow.x, 0);
        assert_eq!(narrow.width, SHELL_MARGIN_MIN_WIDTH - 1);

        let roomy = shell_rect(Rect::new(0, 0, SHELL_MARGIN_MIN_WIDTH, 30));
        assert_eq!(roomy.x, 2);
        assert_eq!(roomy.width, SHELL_MARGIN_MIN_WIDTH - 4);

        let minimum = shell_rect(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT));
        assert_eq!(
            minimum.width, MIN_WIDTH,
            "the minimum size keeps every column"
        );
        assert_eq!(minimum.height, MIN_HEIGHT);
    }
}
