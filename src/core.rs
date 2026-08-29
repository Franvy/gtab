use anyhow::{Context, Result, anyhow, bail};
#[cfg(target_os = "macos")]
use std::ffi::{CStr, c_void};
use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const APPLE_EXT: &str = "applescript";
const DIR_EXT: &str = "path";
const DIRS_DIR_NAME: &str = "dirs";
const DEFAULT_GHOSTTY_SHORTCUT: &str = "cmd+g";
const GHOSTTY_SHORTCUT_INCLUDE_NAME: &str = "ghostty-shortcut.conf";
const SHELL_TRIGGER_GHOSTTY_ESCAPE: &str = "\\x1b[71;9u";
const SHELL_TRIGGER_READLINE_ESCAPE: &str = "\\e[71;9u";
const SHELL_TRIGGER_FISH_ESCAPE: &str = "\\e\\[71\\;9u";
const SHELL_TRIGGER_BASH_CHORD: &str = "\\C-x\\C-g";
const SHELL_INIT_MARKER: &str = "gtab shell-init";
const SHELL_RC_CANDIDATES: [&str; 6] = [
    ".zshrc",
    ".zshenv",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".config/fish/config.fish",
];
const GHOSTTY_EXTERNAL_CONFIG_REASON: &str = "Ghostty config appears to be managed externally (for example by Nix/Home Manager) and was not modified.";
const GHOSTTY_DISCOVERY_ATTEMPTS: usize = 100;
const GHOSTTY_DISCOVERY_DELAY_MS: u64 = 50;
const LEGACY_LAUNCHER_SCRIPT_NAME: &str = "launcher.sh";
const LEGACY_HOTKEY_SERVICE_LABEL: &str = "com.franvy.gtab.hotkey";
const LEGACY_HOTKEY_PLIST_NAME: &str = "com.franvy.gtab.hotkey.plist";
const LEGACY_HOTKEY_LOG_NAME: &str = "gtab-hotkey.log";
const NIX_STORE_ROOT: &str = "/nix/store";
const LAUNCH_WARNING_UNSUPPORTED_CUSTOM_CONFIG: &str = "workspace launched without frame sync; saved AppleScript uses unsupported custom configuration";
const LAUNCH_WARNING_RECONSTRUCTION_FAILED: &str =
    "workspace launched without frame sync; saved AppleScript could not be reconstructed";
#[cfg(target_os = "macos")]
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[cfg(target_os = "macos")]
type Boolean = u8;
#[cfg(target_os = "macos")]
type CFIndex = isize;
#[cfg(target_os = "macos")]
type CFStringEncoding = u32;
#[cfg(target_os = "macos")]
type CFTypeRef = *const c_void;
#[cfg(target_os = "macos")]
type CFStringRef = *const c_void;
#[cfg(target_os = "macos")]
type TISInputSourceRef = *const c_void;

#[cfg(target_os = "macos")]
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    fn TISCopyCurrentASCIICapableKeyboardInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(
        input_source: TISInputSourceRef,
        property_key: CFStringRef,
    ) -> CFTypeRef;
    fn TISSelectInputSource(input_source: TISInputSourceRef) -> i32;
    static kTISPropertyInputSourceID: CFStringRef;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CFTypeRef);
    fn CFStringGetLength(value: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
    fn CFStringGetCString(
        value: CFStringRef,
        buffer: *mut i8,
        buffer_size: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
}

#[derive(Clone, Debug)]
pub struct Config {
    pub close_tab: bool,
    pub ghostty_shortcut: String,
    pub shortcut_mode: ShortcutMode,
}

/// How the Ghostty keybind reaches `gtab`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutMode {
    /// Ghostty types `gtab` into the focused shell. Leaves the command on screen.
    Text,
    /// Ghostty sends an invisible trigger sequence that the `gtab shell-init`
    /// widget binds to, so nothing is echoed and nothing enters shell history.
    Shell,
}

impl ShortcutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Shell => "shell",
        }
    }
}

/// Shells that `gtab shell-init` can emit an integration snippet for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceTab {
    pub title: String,
    pub working_dir: Option<String>,
}

#[derive(Clone, Debug)]
pub enum WorkspacePaneLayout {
    Leaf {
        working_dir: String,
    },
    SplitRight {
        left: Box<WorkspacePaneLayout>,
        right: Box<WorkspacePaneLayout>,
    },
    SplitDown {
        top: Box<WorkspacePaneLayout>,
        bottom: Box<WorkspacePaneLayout>,
    },
}

#[derive(Clone, Debug)]
pub struct WorkspaceTabLayout {
    pub title: String,
    pub root: WorkspacePaneLayout,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    pub tabs: Vec<WorkspaceTab>,
    pub layout: Vec<WorkspaceTabLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedDirectory {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowFrame {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
pub struct ShortcutLauncherInputSourceGuard {
    #[cfg(target_os = "macos")]
    previous_source: Option<MacInputSource>,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MacInputSource {
    raw: TISInputSourceRef,
}

#[derive(Debug)]
pub struct AppEnv {
    pub base_dir: PathBuf,
    pub config_file: PathBuf,
    pub config: Config,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct TabRow {
    // Fields are read by tests and by the cfg(test) `build_workspace_script`
    // helper. Production code only checks `rows.len()` via `plan_workspace_launch`
    // today, but keeping the structured data lets future launch-mode work
    // build on a single parsed representation.
    working_dir: String,
    title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceLaunchMode {
    DirectLegacy,
    DirectSplit,
    DirectFallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedTabSurface {
    tab_index: usize,
    pane_index: usize,
    terminal_id: String,
    working_dir: String,
    title: String,
    rect: CapturedPaneRect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedPaneRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl AppEnv {
    pub fn load() -> Result<Self> {
        let base_dir = resolve_base_dir()?;
        fs::create_dir_all(&base_dir)
            .with_context(|| format!("failed to create {}", base_dir.display()))?;

        let config_file = base_dir.join("config");
        let config = Config::load(&config_file)?;

        Ok(Self {
            base_dir,
            config_file,
            config,
        })
    }

    pub fn reload_config(&mut self) -> Result<()> {
        self.config = Config::load(&self.config_file)?;
        Ok(())
    }

    pub fn set_close_tab(&mut self, enabled: bool) -> Result<()> {
        self.config.close_tab = enabled;
        self.write_config()
    }

    pub fn set_ghostty_shortcut(&mut self, shortcut: &str) -> Result<GhosttyShortcutApplyResult> {
        self.config.ghostty_shortcut = normalize_ghostty_shortcut(shortcut)?;
        self.write_config()?;
        self.sync_ghostty_shortcut()
    }

    pub fn set_shortcut_mode(&mut self, mode: &str) -> Result<GhosttyShortcutApplyResult> {
        self.config.shortcut_mode = normalize_shortcut_mode(mode)?;
        self.write_config()?;
        self.sync_ghostty_shortcut()
    }

    pub fn ensure_ghostty_shortcut(&self) -> Result<GhosttyShortcutApplyResult> {
        let sync = self.preview_ghostty_shortcut_sync();
        self.sync_ghostty_shortcut_files(&sync)
    }

    pub fn init_shortcuts(&mut self) -> Result<GhosttyShortcutApplyResult> {
        self.config.ghostty_shortcut = DEFAULT_GHOSTTY_SHORTCUT.to_string();
        if self.shell_integration_installed() {
            self.config.shortcut_mode = ShortcutMode::Shell;
        }
        self.write_config()?;
        let sync = self.sync_ghostty_shortcut()?;
        self.cleanup_legacy_shortcut_artifacts().ok();
        Ok(sync)
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let mut workspaces = Vec::new();

        for entry in fs::read_dir(&self.base_dir)
            .with_context(|| format!("failed to read {}", self.base_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(APPLE_EXT) {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };

            let content = fs::read_to_string(&path).ok();
            let tabs = content
                .as_deref()
                .map(parse_workspace_tabs)
                .unwrap_or_default();
            let layout = content
                .as_deref()
                .map(parse_workspace_layout)
                .unwrap_or_default();

            workspaces.push(Workspace {
                name: stem.to_string(),
                path,
                tabs,
                layout,
            });
        }

        workspaces.sort_by_key(|workspace| workspace.name.to_lowercase());
        Ok(workspaces)
    }

    pub fn list_directories(&self) -> Result<Vec<SavedDirectory>> {
        let mut directories = Vec::new();
        let directories_dir = self.base_dir.join(DIRS_DIR_NAME);
        if !directories_dir.exists() {
            return Ok(directories);
        }

        for entry in fs::read_dir(&directories_dir)
            .with_context(|| format!("failed to read {}", directories_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(DIR_EXT) {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };

            let saved_path = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let saved_path = saved_path.trim_end_matches(['\r', '\n']);
            if saved_path.is_empty() {
                continue;
            }

            directories.push(SavedDirectory {
                name: stem.to_string(),
                path: PathBuf::from(saved_path),
            });
        }

        directories.sort_by_key(|directory| directory.name.to_lowercase());
        Ok(directories)
    }

    pub fn workspace_path(&self, name: &str) -> Result<PathBuf> {
        validate_workspace_name(name)?;
        Ok(self.base_dir.join(format!("{name}.{APPLE_EXT}")))
    }

    pub fn directory_path(&self, name: &str) -> Result<PathBuf> {
        validate_workspace_name(name)?;
        Ok(self
            .base_dir
            .join(DIRS_DIR_NAME)
            .join(format!("{name}.{DIR_EXT}")))
    }

    pub fn save_current_window(&self, name: &str) -> Result<PathBuf> {
        let path = self.workspace_path(name)?;
        let script = capture_workspace_script()?;
        fs::write(&path, &script).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub fn save_directory(&self, name: &str, directory_path: &Path) -> Result<PathBuf> {
        let path = self.directory_path(name)?;
        if path.exists() {
            bail!("directory '{name}' already exists");
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(&path, directory_path.to_string_lossy().as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub fn open_in_editor(&self, name: &str) -> Result<()> {
        let path = self.workspace_path(name)?;
        let editor = env::var("EDITOR")
            .ok()
            .filter(|editor| !editor.trim().is_empty())
            .unwrap_or_else(|| "vim".to_string());

        let status = Command::new(&editor)
            .arg(&path)
            .status()
            .with_context(|| format!("failed to launch editor {editor}"))?;

        if !status.success() {
            bail!("editor exited with status {status}");
        }

        Ok(())
    }

    pub fn rename_workspace(&self, old_name: &str, new_name: &str) -> Result<PathBuf> {
        let old_path = self.workspace_path(old_name)?;
        if !old_path.exists() {
            bail!("workspace '{old_name}' not found");
        }

        if old_name == new_name {
            return Ok(old_path);
        }

        let new_path = self.workspace_path(new_name)?;
        if new_path.exists() {
            bail!("workspace '{new_name}' already exists");
        }

        fs::rename(&old_path, &new_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        Ok(new_path)
    }

    pub fn remove_workspace(&self, name: &str) -> Result<PathBuf> {
        let path = self.workspace_path(name)?;
        if !path.exists() {
            bail!("workspace '{name}' not found");
        }

        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        Ok(path)
    }

    pub fn rename_directory(&self, old_name: &str, new_name: &str) -> Result<PathBuf> {
        let old_path = self.directory_path(old_name)?;
        if !old_path.exists() {
            bail!("directory '{old_name}' not found");
        }

        if old_name == new_name {
            return Ok(old_path);
        }

        let new_path = self.directory_path(new_name)?;
        if new_path.exists() {
            bail!("directory '{new_name}' already exists");
        }

        fs::rename(&old_path, &new_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        Ok(new_path)
    }

    pub fn remove_directory(&self, name: &str) -> Result<PathBuf> {
        let path = self.directory_path(name)?;
        if !path.exists() {
            bail!("directory '{name}' not found");
        }

        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        Ok(path)
    }

    pub fn validate_directory_target(&self, path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() {
            bail!("directory path is empty");
        }

        if !path.exists() {
            bail!("directory '{}' not found", path.display());
        }

        if !path.is_dir() {
            bail!("'{}' is not a directory", path.display());
        }

        Ok(())
    }

    pub fn open_directory_in_focused_terminal(&self, path: &Path) -> Result<()> {
        self.validate_directory_target(path)?;
        let command = render_ghostty_direct_cd_command(path);
        let script = build_ghostty_cd_script(&command);
        run_osascript(&script).with_context(
            || "failed to open directory in Ghostty (check Automation permissions for osascript)",
        )?;
        Ok(())
    }

    pub fn replace_directory_in_focused_terminal(&self, path: &Path) -> Result<()> {
        self.validate_directory_target(path)?;
        let script = build_ghostty_replace_directory_script(path);
        run_osascript(&script).with_context(|| {
            "failed to replace the focused Ghostty split (check Automation permissions for osascript)"
        })?;
        Ok(())
    }

    pub fn launch_workspace(&self, name: &str) -> Result<()> {
        let path = self.workspace_path(name)?;
        if !path.exists() {
            bail!("workspace '{name}' not found");
        }

        self.launch_workspace_script_path(&path)?;
        self.finish_workspace_launch()?;
        Ok(())
    }

    pub fn launch_workspace_from_tui_with_frame(
        &self,
        name: &str,
        frame: &WindowFrame,
    ) -> Result<Option<String>> {
        let path = self.workspace_path(name)?;
        if !path.exists() {
            bail!("workspace '{name}' not found");
        }

        let script = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let rows = parse_workspace_rows(&script);
        match plan_workspace_launch(&script, &rows) {
            WorkspaceLaunchMode::DirectLegacy => {
                self.launch_workspace_script_path(&path)?;
                self.finish_workspace_launch()?;
                Ok(Some(LAUNCH_WARNING_UNSUPPORTED_CUSTOM_CONFIG.to_string()))
            }
            WorkspaceLaunchMode::DirectSplit => {
                let existing_window_ids = ghostty_window_ids()?;
                self.launch_workspace_script_path(&path)?;
                if let Ok(window_id) = wait_for_new_ghostty_window_id(&existing_window_ids) {
                    let _ = reposition_ghostty_window(&window_id, frame);
                }
                self.finish_workspace_launch()?;
                Ok(None)
            }
            WorkspaceLaunchMode::DirectFallback => {
                self.launch_workspace_script_path(&path)?;
                self.finish_workspace_launch()?;
                Ok(Some(LAUNCH_WARNING_RECONSTRUCTION_FAILED.to_string()))
            }
        }
    }

    pub fn capture_frontmost_ghostty_window_frame(&self) -> Result<WindowFrame> {
        capture_ghostty_window_frame()
    }

    pub fn close_tab_display(&self) -> &'static str {
        if self.config.close_tab { "on" } else { "off" }
    }

    pub fn ghostty_shortcut_display(&self) -> &str {
        &self.config.ghostty_shortcut
    }

    pub fn shortcut_mode(&self) -> ShortcutMode {
        self.config.shortcut_mode
    }

    pub fn shortcut_mode_display(&self) -> &'static str {
        match self.config.shortcut_mode {
            ShortcutMode::Text => "text (types `gtab` into the shell)",
            ShortcutMode::Shell => "shell (silent widget trigger)",
        }
    }

    /// True when a `gtab shell-init` line is present in one of the user's shell rc files.
    pub fn shell_integration_installed(&self) -> bool {
        home_dir()
            .map(|home| shell_integration_installed(&home))
            .unwrap_or(false)
    }

    fn launch_workspace_script_path(&self, path: &Path) -> Result<()> {
        let output = Command::new("osascript")
            .arg(path)
            .output()
            .with_context(|| format!("failed to run {}", path.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = stderr.trim();
            if msg.is_empty() {
                bail!("workspace launch failed");
            } else {
                bail!("workspace launch failed: {msg}");
            }
        }

        Ok(())
    }

    fn finish_workspace_launch(&self) -> Result<()> {
        if self.config.close_tab {
            hup_parent_process()?;
        }

        Ok(())
    }

    pub fn preview_ghostty_shortcut_sync(&self) -> GhosttyShortcutSync {
        GhosttyShortcutSync {
            config_path: resolve_ghostty_config_path().unwrap_or_else(|_| {
                home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config/ghostty/config.ghostty")
            }),
            include_path: self.base_dir.join(GHOSTTY_SHORTCUT_INCLUDE_NAME),
            shortcut: self.config.ghostty_shortcut.clone(),
            mode: self.config.shortcut_mode,
        }
    }

    fn sync_ghostty_shortcut(&self) -> Result<GhosttyShortcutApplyResult> {
        let sync = self.preview_ghostty_shortcut_sync();
        self.sync_ghostty_shortcut_files(&sync)
    }

    fn sync_ghostty_shortcut_files(
        &self,
        sync: &GhosttyShortcutSync,
    ) -> Result<GhosttyShortcutApplyResult> {
        if is_shortcut_disabled(&self.config.ghostty_shortcut) {
            let config_result =
                sync_ghostty_include_reference(&sync.config_path, &sync.include_path, false)?;
            let include_removed = remove_file_if_exists(&sync.include_path)?;
            let (status, reason) = match config_result {
                GhosttyConfigSync::Updated => (GhosttyShortcutApplyStatus::UpdatedConfig, None),
                GhosttyConfigSync::Unchanged => {
                    let status = if include_removed {
                        GhosttyShortcutApplyStatus::UpdatedConfig
                    } else {
                        GhosttyShortcutApplyStatus::AlreadyConfigured
                    };
                    (status, None)
                }
                GhosttyConfigSync::ManualConfigRequired { reason } => (
                    GhosttyShortcutApplyStatus::ManualConfigRemovalRequired,
                    Some(reason),
                ),
            };
            return Ok(GhosttyShortcutApplyResult {
                sync: sync.clone(),
                status,
                reason,
            });
        }

        let include_changed = self.write_ghostty_shortcut_include(&sync.include_path)?;
        let config_result =
            sync_ghostty_include_reference(&sync.config_path, &sync.include_path, true)?;
        let (status, reason) = match config_result {
            GhosttyConfigSync::Updated => (GhosttyShortcutApplyStatus::UpdatedConfig, None),
            GhosttyConfigSync::Unchanged => {
                let status = if include_changed {
                    GhosttyShortcutApplyStatus::UpdatedConfig
                } else {
                    GhosttyShortcutApplyStatus::AlreadyConfigured
                };
                (status, None)
            }
            GhosttyConfigSync::ManualConfigRequired { reason } => (
                GhosttyShortcutApplyStatus::ManualConfigRequired,
                Some(reason),
            ),
        };

        Ok(GhosttyShortcutApplyResult {
            sync: sync.clone(),
            status,
            reason,
        })
    }

    fn write_ghostty_shortcut_include(&self, path: &Path) -> Result<bool> {
        let content = build_ghostty_shortcut_include(
            &self.config.ghostty_shortcut,
            self.config.shortcut_mode,
        );
        if fs::read_to_string(path).ok().as_deref() == Some(content.as_str()) {
            return Ok(false);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(true)
    }

    fn launchctl_domain(&self) -> Result<String> {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .context("failed to resolve current user id")?;

        if !output.status.success() {
            bail!("failed to resolve current user id");
        }

        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if uid.is_empty() {
            bail!("failed to resolve current user id");
        }

        Ok(format!("gui/{uid}"))
    }

    fn legacy_hotkey_plist_path(&self) -> Result<PathBuf> {
        let home = home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(LEGACY_HOTKEY_PLIST_NAME))
    }

    fn bootout_legacy_hotkey_agent(&self) -> Result<()> {
        let plist_path = self.legacy_hotkey_plist_path()?;
        let domain = self.launchctl_domain()?;
        let status = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&plist_path)
            .status()
            .with_context(|| format!("failed to stop {}", plist_path.display()))?;

        if !status.success() {
            bail!("failed to stop legacy hotkey agent");
        }

        Ok(())
    }

    fn legacy_hotkey_loaded(&self) -> bool {
        let Ok(domain) = self.launchctl_domain() else {
            return false;
        };

        match Command::new("launchctl")
            .args(["print", &format!("{domain}/{LEGACY_HOTKEY_SERVICE_LABEL}")])
            .output()
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    fn cleanup_legacy_shortcut_artifacts(&self) -> Result<()> {
        if self.legacy_hotkey_loaded() {
            self.bootout_legacy_hotkey_agent().ok();
        }

        let plist_path = self.legacy_hotkey_plist_path()?;
        if plist_path.exists() {
            fs::remove_file(&plist_path)
                .with_context(|| format!("failed to remove {}", plist_path.display()))?;
        }

        for path in [
            self.base_dir.join(LEGACY_HOTKEY_LOG_NAME),
            self.base_dir.join(LEGACY_LAUNCHER_SCRIPT_NAME),
        ] {
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }

        Ok(())
    }

    fn write_config(&mut self) -> Result<()> {
        if let Some(parent) = self.config_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(&self.config_file, self.config.serialize())
            .with_context(|| format!("failed to write {}", self.config_file.display()))?;
        self.reload_config()
    }
}

impl ShortcutLauncherInputSourceGuard {
    pub fn activate_for_tui() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let current = MacInputSource::current_keyboard()
                .context("failed to resolve the current macOS input source")?;
            let ascii = MacInputSource::current_ascii_capable()
                .context("failed to resolve an ASCII-capable macOS input source")?;

            let should_switch = should_switch_to_ascii_input_source(
                current.id().ok().as_deref(),
                ascii.id().ok().as_deref(),
                current.ptr_eq(&ascii),
            );

            if !should_switch {
                return Ok(Self {
                    previous_source: None,
                });
            }

            ascii
                .select()
                .context("failed to switch gtab to an ASCII-capable input source")?;

            Ok(Self {
                previous_source: Some(current),
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self {})
        }
    }

    fn restore(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let Some(previous_source) = self.previous_source.take() else {
                return Ok(());
            };

            previous_source
                .select()
                .context("failed to restore the previous macOS input source")?;
        }

        Ok(())
    }
}

impl Drop for ShortcutLauncherInputSourceGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("warning: {error}");
        }
    }
}

#[cfg(target_os = "macos")]
impl MacInputSource {
    fn current_keyboard() -> Result<Self> {
        let raw = unsafe { TISCopyCurrentKeyboardInputSource() };
        Self::new(raw, "current keyboard input source was unavailable")
    }

    fn current_ascii_capable() -> Result<Self> {
        let raw = unsafe { TISCopyCurrentASCIICapableKeyboardInputSource() };
        Self::new(raw, "ASCII-capable keyboard input source was unavailable")
    }

    fn new(raw: TISInputSourceRef, context: &str) -> Result<Self> {
        if raw.is_null() {
            bail!("{context}");
        }

        Ok(Self { raw })
    }

    fn id(&self) -> Result<String> {
        let raw = unsafe { TISGetInputSourceProperty(self.raw, kTISPropertyInputSourceID) };
        cf_string_to_string(raw as CFStringRef)
            .context("failed to read the input source identifier")
    }

    fn ptr_eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }

    fn select(&self) -> Result<()> {
        let status = unsafe { TISSelectInputSource(self.raw) };
        if status == 0 {
            return Ok(());
        }

        bail!("macOS returned OSStatus {status} while selecting an input source")
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacInputSource {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { CFRelease(self.raw) };
        }
    }
}

fn should_switch_to_ascii_input_source(
    current_id: Option<&str>,
    ascii_id: Option<&str>,
    same_source: bool,
) -> bool {
    if same_source {
        return false;
    }

    match (current_id, ascii_id) {
        (Some(current_id), Some(ascii_id)) => current_id != ascii_id,
        _ => true,
    }
}

#[cfg(target_os = "macos")]
fn cf_string_to_string(value: CFStringRef) -> Result<String> {
    if value.is_null() {
        bail!("CFString value was null");
    }

    let length = unsafe { CFStringGetLength(value) };
    let buffer_size =
        unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) };
    if buffer_size < 0 {
        bail!("failed to size the UTF-8 input source buffer");
    }

    let mut buffer = vec![0_i8; buffer_size as usize + 1];
    let ok = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        bail!("failed to decode the input source identifier as UTF-8");
    }

    let string = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Ok(string.to_string_lossy().into_owned())
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config = Self::default();

        for line in raw.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            if key.trim() == "close_tab" {
                config.close_tab = matches!(value.trim(), "true" | "on");
            } else if key.trim() == "ghostty_shortcut" && !value.trim().is_empty() {
                config.ghostty_shortcut = normalize_ghostty_shortcut(value.trim())?;
            } else if key.trim() == "shortcut_mode" && !value.trim().is_empty() {
                config.shortcut_mode = normalize_shortcut_mode(value.trim())?;
            }
        }

        Ok(config)
    }

    fn serialize(&self) -> String {
        let close_tab = if self.close_tab { "true" } else { "false" };
        format!(
            "close_tab={close_tab}\nghostty_shortcut={}\nshortcut_mode={}\n",
            self.ghostty_shortcut,
            self.shortcut_mode.as_str(),
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            close_tab: false,
            ghostty_shortcut: DEFAULT_GHOSTTY_SHORTCUT.to_string(),
            shortcut_mode: ShortcutMode::Text,
        }
    }
}

fn resolve_base_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("GTAB_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let home = home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
    Ok(home.join(".config").join("gtab"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn resolve_ghostty_config_path() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
    let xdg_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let ghostty_dir = xdg_dir.join("ghostty");
    let config_ghostty = ghostty_dir.join("config.ghostty");
    let legacy_config = ghostty_dir.join("config");

    if config_ghostty.exists() {
        return Ok(config_ghostty);
    }

    if legacy_config.exists() {
        return Ok(legacy_config);
    }

    Ok(config_ghostty)
}

fn validate_workspace_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("missing workspace name");
    }

    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\0') {
        bail!("invalid workspace name '{name}'");
    }

    Ok(())
}

fn normalize_ghostty_shortcut(shortcut: &str) -> Result<String> {
    let normalized = shortcut.trim().to_lowercase();
    if normalized.is_empty() {
        bail!("ghostty_shortcut cannot be empty");
    }

    if is_shortcut_disabled(&normalized) {
        return Ok("off".to_string());
    }

    if normalized.contains('=') || normalized.contains('\n') || normalized.contains('\r') {
        bail!("ghostty_shortcut contains invalid characters");
    }

    Ok(normalized)
}

fn normalize_shortcut_mode(mode: &str) -> Result<ShortcutMode> {
    match mode.trim().to_lowercase().as_str() {
        "text" | "type" => Ok(ShortcutMode::Text),
        "shell" | "widget" => Ok(ShortcutMode::Shell),
        "" => bail!("shortcut_mode cannot be empty"),
        other => bail!("shortcut_mode value must be 'shell' or 'text', got '{other}'"),
    }
}

fn parse_shell_kind(shell: &str) -> Result<ShellKind> {
    let name = shell
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_start_matches('-')
        .to_lowercase();

    match name.as_str() {
        "zsh" => Ok(ShellKind::Zsh),
        "bash" => Ok(ShellKind::Bash),
        "fish" => Ok(ShellKind::Fish),
        "" => bail!("missing shell name; use `gtab shell-init zsh|bash|fish`"),
        other => bail!("unsupported shell '{other}'; use zsh, bash, or fish"),
    }
}

/// Resolves the shell for `gtab shell-init` when no shell argument is given.
pub fn detect_shell_kind() -> Result<ShellKind> {
    let shell = env::var("SHELL").unwrap_or_default();
    if shell.trim().is_empty() {
        bail!("could not detect the current shell; use `gtab shell-init zsh|bash|fish`");
    }

    parse_shell_kind(&shell)
}

/// Resolves a shell name coming from the CLI, falling back to `$SHELL`.
pub fn resolve_shell_kind(requested: Option<&str>) -> Result<ShellKind> {
    match requested {
        Some(shell) => parse_shell_kind(shell),
        None => detect_shell_kind(),
    }
}

/// Shell snippet that binds the invisible Ghostty trigger to a `gtab` launcher,
/// so the shortcut never echoes `gtab` into the prompt or shell history.
pub fn render_shell_init(shell: ShellKind) -> String {
    match shell {
        ShellKind::Zsh => format!(
            r#"# gtab shell integration (zsh)
# Add to ~/.zshrc:  eval "$(gtab shell-init zsh)"
if [[ -o interactive ]]; then
  _gtab_launch_widget() {{
    # zle detaches stdin from the terminal, and macOS kqueue cannot watch the
    # /dev/tty clone device, so the TUI needs the real device path from $TTY.
    local gtab_tty=${{TTY:-$(command tty 2>/dev/null)}}
    if [[ -c $gtab_tty ]]; then
      command gtab <$gtab_tty >$gtab_tty 2>&1
    else
      command gtab
    fi
    local ret=$?
    zle reset-prompt
    return $ret
  }}
  zle -N _gtab_launch_widget
  bindkey '{seq}' _gtab_launch_widget
  bindkey -M viins '{seq}' _gtab_launch_widget 2>/dev/null
  bindkey -M vicmd '{seq}' _gtab_launch_widget 2>/dev/null
fi
"#,
            seq = SHELL_TRIGGER_READLINE_ESCAPE
        ),
        ShellKind::Bash => format!(
            r#"# gtab shell integration (bash)
# Add to ~/.bashrc:  eval "$(gtab shell-init bash)"
if [[ $- == *i* ]]; then
  _gtab_launch_widget() {{
    # readline hands the function the real tty; never redirect to /dev/tty,
    # because macOS kqueue cannot watch that clone device.
    local gtab_tty
    gtab_tty=$(command tty 2>/dev/null)
    if [[ -c $gtab_tty ]]; then
      command gtab <"$gtab_tty" >"$gtab_tty" 2>&1
    else
      command gtab
    fi
  }}
  # bash 3.2 (the macOS system bash) cannot run a function bound straight to an
  # escape sequence, so the trigger is a macro that fires a short chord instead.
  bind -m emacs-standard -x '"{chord}": _gtab_launch_widget' 2>/dev/null
  bind -m vi-insert -x '"{chord}": _gtab_launch_widget' 2>/dev/null
  bind -m vi-command -x '"{chord}": _gtab_launch_widget' 2>/dev/null
  bind -m emacs-standard '"{seq}": "{chord}"' 2>/dev/null
  bind -m vi-insert '"{seq}": "{chord}"' 2>/dev/null
  bind -m vi-command '"{seq}": "{chord}"' 2>/dev/null
fi
"#,
            seq = SHELL_TRIGGER_READLINE_ESCAPE,
            chord = SHELL_TRIGGER_BASH_CHORD
        ),
        ShellKind::Fish => format!(
            r#"# gtab shell integration (fish)
# Add to ~/.config/fish/config.fish:  gtab shell-init fish | source
function _gtab_launch_widget
    # macOS kqueue cannot watch the /dev/tty clone device, so pass the real one.
    set -l gtab_tty (command tty 2>/dev/null)
    if test -c "$gtab_tty"
        command gtab <$gtab_tty >$gtab_tty 2>&1
    else
        command gtab
    end
    commandline -f repaint
end

if status is-interactive
    bind {seq} _gtab_launch_widget
    bind -M insert {seq} _gtab_launch_widget 2>/dev/null
end
"#,
            seq = SHELL_TRIGGER_FISH_ESCAPE
        ),
    }
}

/// Best-effort check for an installed `gtab shell-init` line in the user's shell rc files.
fn shell_integration_installed(home: &Path) -> bool {
    SHELL_RC_CANDIDATES.iter().any(|candidate| {
        fs::read_to_string(home.join(candidate))
            .map(|contents| {
                contents
                    .lines()
                    .filter(|line| !line.trim_start().starts_with('#'))
                    .any(|line| line.contains(SHELL_INIT_MARKER))
            })
            .unwrap_or(false)
    })
}

fn render_shell_cd_command(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("cd -- '{}'", shell_single_quote_escape(&raw))
}

fn render_ghostty_direct_cd_command(path: &Path) -> String {
    let cd_command = render_shell_cd_command(path);
    format!(" {cd_command}")
}

fn shell_single_quote_escape(value: &str) -> String {
    value.replace('\'', r#"'"'"'"#)
}

fn build_ghostty_cd_script(command: &str) -> String {
    format!(
        "tell application \"Ghostty\"\n    if (count of windows) is 0 then error \"Ghostty has no open window\"\n    set term to focused terminal of selected tab of front window\n    input text \"{}\" to term\n    send key \"enter\" to term\nend tell",
        apple_escape(command)
    )
}

fn build_ghostty_replace_directory_script(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!(
        "tell application \"Ghostty\"\n    if (count of windows) is 0 then error \"Ghostty has no open window\"\n    set term to focused terminal of selected tab of front window\n    set cfg to new surface configuration\n    set initial working directory of cfg to \"{}\"\n    set newTerm to split term direction right with configuration cfg\n    close term\n    focus newTerm\nend tell",
        apple_escape(&path)
    )
}

fn is_shortcut_disabled(shortcut: &str) -> bool {
    matches!(shortcut.trim(), "off" | "none" | "disabled")
}

fn capture_workspace_script() -> Result<String> {
    // Phase 1: read all tabs/panes; for multi-pane tabs also capture AX screen positions
    let raw = run_osascript(
        r#"set D to (ASCII character 9)
tell application "Ghostty"
  set win to front window
  set tabList to tabs of win
  set allLines to {}
  repeat with ti from 1 to count of tabList
    set t to item ti of tabList
    set termList to every terminal of t
    set ttl to name of t
    if (count of termList) = 1 then
      set term to item 1 of termList
      set wd to working directory of term
      if wd = "" then set wd to POSIX path of (path to home folder)
      set end of allLines to (ti as text) & D & "1" & D & (id of term as text) & D & wd & D & ttl & D & "0,0,1,1"
    else
      repeat with pi from 1 to count of termList
        set term to item pi of termList
        set wd to working directory of term
        if wd = "" then set wd to POSIX path of (path to home folder)
        focus term
        delay 0.12
        set posStr to "0,0,1,1"
        tell application "System Events"
          try
            set gProc to application process "Ghostty"
            set fe to value of attribute "AXFocusedUIElement" of gProc
            repeat 8 times
              if role of fe = "AXScrollArea" then exit repeat
              set fe to value of attribute "AXParent" of fe
            end repeat
            set pos to position of fe
            set sz to size of fe
            set posStr to ((item 1 of pos) as text) & "," & ((item 2 of pos) as text) & "," & ((item 1 of sz) as text) & "," & ((item 2 of sz) as text)
          end try
        end tell
        set end of allLines to (ti as text) & D & (pi as text) & D & (id of term as text) & D & wd & D & ttl & D & posStr
      end repeat
    end if
  end repeat
  set AppleScript's text item delimiters to linefeed
  return allLines as text
end tell"#,
    )
    .context("could not read Ghostty tabs (make sure Ghostty is the frontmost app)")?;

    if raw.trim().is_empty() {
        bail!("could not read Ghostty tabs (make sure Ghostty is the frontmost app)");
    }

    let captured = parse_captured_tab_surfaces(&raw);
    if captured.is_empty() {
        bail!("could not parse Ghostty tabs (make sure Ghostty is the frontmost app)");
    }

    let normalized = serialize_captured_tab_surfaces(&captured);

    // Phase 2: reconstruct split trees from positions and generate restore script
    let py = r#"import sys
from collections import defaultdict

def esc(s):
    return s.replace('\\', '\\\\').replace('"', '\\"')

def get_anchor(tree):
    if tree['t'] == 'leaf': return tree['p']
    if tree['t'] == 'v':    return get_anchor(tree['l'])
    return get_anchor(tree['T'])

def reconstruct(panes):
    if len(panes) == 1:
        return {'t': 'leaf', 'p': panes[0]}
    for sx in sorted(set(p['x'] + p['w'] for p in panes)):
        L = [p for p in panes if p['x'] + p['w'] <= sx + 2]
        R = [p for p in panes if p['x'] >= sx - 2]
        if L and R and len(L) + len(R) == len(panes):
            return {'t': 'v', 'l': reconstruct(L), 'r': reconstruct(R)}
    for sy in sorted(set(p['y'] + p['h'] for p in panes)):
        T = [p for p in panes if p['y'] + p['h'] <= sy + 2]
        B = [p for p in panes if p['y'] >= sy - 2]
        if T and B and len(T) + len(B) == len(panes):
            return {'t': 'h', 'T': reconstruct(T), 'B': reconstruct(B)}
    return {'t': 'leaf', 'p': panes[0]}

def gen(tree, var, lines, c):
    if tree['t'] == 'leaf': return c
    if tree['t'] == 'v':
        a = get_anchor(tree['r'])
        lines += [
            '',
            f"    set cfg{c} to new surface configuration",
            f'    set initial working directory of cfg{c} to "{esc(a["wd"])}"',
            f"    set p{c} to split {var} direction right with configuration cfg{c}"
        ]
        rv, c = f"p{c}", c + 1
        c = gen(tree['l'], var, lines, c)
        c = gen(tree['r'], rv, lines, c)
    else:
        a = get_anchor(tree['B'])
        lines += [
            '',
            f"    set cfg{c} to new surface configuration",
            f'    set initial working directory of cfg{c} to "{esc(a["wd"])}"',
            f"    set p{c} to split {var} direction down with configuration cfg{c}"
        ]
        bv, c = f"p{c}", c + 1
        c = gen(tree['T'], var, lines, c)
        c = gen(tree['B'], bv, lines, c)
    return c

tabs = defaultdict(lambda: {'title': '', 'panes': []})
for line in sys.stdin:
    parts = line.rstrip('\n').split('\t')
    if len(parts) < 6: continue
    ti, wd, title, pos = int(parts[0]), parts[3], parts[4], parts[5]
    try: x, y, w, h = map(int, pos.split(','))
    except ValueError: x, y, w, h = 0, 0, 1, 1
    tabs[ti]['title'] = title
    tabs[ti]['panes'].append({'x': x, 'y': y, 'w': w, 'h': h, 'wd': wd})

out = ['tell application "Ghostty"', '    activate']
c = 1
for i, ti in enumerate(sorted(tabs.keys())):
    tab = tabs[ti]
    tree = reconstruct(tab['panes'])
    anchor = get_anchor(tree)
    out += [
        '',
        f"    set cfg{c} to new surface configuration",
        f'    set initial working directory of cfg{c} to "{esc(anchor["wd"])}"'
    ]
    if i == 0:
        out += [
            f"    set win to new window with configuration cfg{c}",
            f"    set p{c} to focused terminal of selected tab of win"
        ]
    else:
        out += [
            f"    set newtab{i} to new tab in win with configuration cfg{c}",
            f"    set p{c} to focused terminal of newtab{i}"
        ]
    fv, c = f"p{c}", c + 1
    if tab['title']:
        out.append(f'    perform action "set_tab_title:{esc(tab["title"])}" on {fv}')
    c = gen(tree, fv, out, c)

out.append('end tell')
print('\n'.join(out))
"#;

    let mut child = Command::new("python3")
        .arg("-c")
        .arg(py)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn python3 for workspace script generation")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open python3 stdin")?;
        stdin
            .write_all(normalized.as_bytes())
            .context("failed to write tab data to python3")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for python3")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to generate workspace script: {stderr}");
    }

    String::from_utf8(output.stdout).context("python3 output was not valid UTF-8")
}

fn parse_captured_tab_surfaces(raw: &str) -> Vec<CapturedTabSurface> {
    raw.lines().filter_map(parse_captured_tab_surface).collect()
}

fn parse_captured_tab_surface(line: &str) -> Option<CapturedTabSurface> {
    let mut parts = line.split('\t');
    let tab_index = parts.next()?.trim().parse().ok()?;
    let pane_index = parts.next()?.trim().parse().ok()?;
    let terminal_id = parts.next()?.trim().to_string();
    let working_dir = parts.next()?.to_string();
    let title = normalize_captured_tab_title(parts.next()?);
    let rect = parse_captured_pane_rect(parts.next()?)?;
    Some(CapturedTabSurface {
        tab_index,
        pane_index,
        terminal_id,
        working_dir,
        title,
        rect,
    })
}

fn parse_captured_pane_rect(value: &str) -> Option<CapturedPaneRect> {
    let mut parts = value.split(',').map(str::trim);
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let width = parts.next()?.parse().ok()?;
    let height = parts.next()?.parse().ok()?;

    Some(CapturedPaneRect {
        x,
        y,
        width,
        height,
    })
}

fn serialize_captured_tab_surfaces(rows: &[CapturedTabSurface]) -> String {
    let mut out = String::new();

    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }

        out.push_str(&row.tab_index.to_string());
        out.push('\t');
        out.push_str(&row.pane_index.to_string());
        out.push('\t');
        out.push_str(&row.terminal_id);
        out.push('\t');
        out.push_str(&row.working_dir);
        out.push('\t');
        let title_to_write = if looks_like_shell_default_title(&row.title, &row.working_dir) {
            ""
        } else {
            &row.title
        };
        out.push_str(title_to_write);
        out.push('\t');
        out.push_str(&format!(
            "{},{},{},{}",
            row.rect.x, row.rect.y, row.rect.width, row.rect.height
        ));
    }

    out
}

fn normalize_captured_tab_title(raw: &str) -> String {
    let mut title = raw.trim().to_string();

    loop {
        let stripped = strip_spinner_prefix(&title)
            .or_else(|| title.strip_prefix("🔔 ").map(str::to_string))
            .map(|value| value.trim_start().to_string());

        match stripped {
            Some(next) if next != title => title = next,
            _ => break,
        }
    }

    title.trim().to_string()
}

fn strip_spinner_prefix(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let first = chars.next()?;
    let is_braille_spinner = matches!(first as u32, 0x2800..=0x28ff);
    if !is_braille_spinner {
        return None;
    }

    let rest = chars.as_str();
    let trimmed = rest.trim_start_matches(char::is_whitespace);
    if trimmed.len() == rest.len() {
        return None;
    }

    Some(trimmed.to_string())
}

fn looks_like_shell_default_title(raw: &str, _working_dir: &str) -> bool {
    let title = normalize_captured_tab_title(raw);
    if title.is_empty() {
        return true;
    }

    // user@host:path pattern (e.g. "fran@MacBook:~/project")
    if let Some(at_pos) = title.find('@') {
        if title[at_pos + 1..].contains(':') {
            return true;
        }
    }

    // path-like patterns
    if title.starts_with("~/") || title == "~" {
        return true;
    }
    if title.starts_with("…/") {
        return true;
    }
    if title.starts_with('/') {
        return true;
    }

    // common shell process names
    matches!(
        title.as_str(),
        "zsh" | "bash" | "fish" | "sh" | "-zsh" | "-bash"
    )
}

#[cfg(test)]
fn build_workspace_script(rows: &[TabRow]) -> String {
    let mut out = String::from("tell application \"Ghostty\"\n    activate");

    for (index, row) in rows.iter().enumerate() {
        let n = index + 1;
        out.push_str("\n\n");
        out.push_str(&format!("    set cfg{n} to new surface configuration\n"));
        out.push_str(&format!(
            "    set initial working directory of cfg{n} to \"{}\"\n",
            apple_escape(&row.working_dir)
        ));

        if index == 0 {
            out.push_str(&format!(
                "    set win to new window with configuration cfg{n}\n"
            ));
            out.push_str(&format!(
                "    set term{n} to focused terminal of selected tab of win"
            ));
        } else {
            out.push_str(&format!(
                "    set tab{n} to new tab in win with configuration cfg{n}\n"
            ));
            out.push_str(&format!("    set term{n} to focused terminal of tab{n}"));
        }

        if !row.title.is_empty() && !looks_like_shell_default_title(&row.title, &row.working_dir) {
            out.push_str(&format!(
                "\n    perform action \"set_tab_title:{}\" on term{n}",
                apple_escape(&row.title)
            ));
        }
    }

    out.push_str("\nend tell\n");
    out
}

fn reposition_ghostty_window(window_id: &str, frame: &WindowFrame) -> Result<()> {
    let script = format!(
        r#"tell application "System Events"
  tell process "Ghostty"
    set matchingWindow to missing value
    repeat with candidate in windows
      try
        if value of attribute "AXIdentifier" of candidate is "{id}" then
          set matchingWindow to candidate
          exit repeat
        end if
      end try
    end repeat
    if matchingWindow is missing value then
      if (count of windows) is 0 then error "Ghostty has no visible window"
      set matchingWindow to window 1
    end if
    set position of matchingWindow to {{{x}, {y}}}
    set size of matchingWindow to {{{w}, {h}}}
  end tell
end tell"#,
        id = apple_escape(window_id),
        x = frame.x,
        y = frame.y,
        w = frame.width,
        h = frame.height
    );
    run_osascript(&script)
        .context("failed to reposition the Ghostty window (check Accessibility permissions)")?;
    Ok(())
}

fn ghostty_window_ids() -> Result<BTreeSet<String>> {
    let output = run_osascript(
        r#"set rows to {}
tell application "Ghostty"
  repeat with win in windows
    set end of rows to id of win
  end repeat
end tell
set AppleScript's text item delimiters to linefeed
return rows as text"#,
    )
    .context("failed to query Ghostty windows")?;

    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn wait_for_new_ghostty_window_id(existing_window_ids: &BTreeSet<String>) -> Result<String> {
    for _ in 0..GHOSTTY_DISCOVERY_ATTEMPTS {
        let current_window_ids = ghostty_window_ids()?;
        if let Some(window_id) = current_window_ids
            .iter()
            .find(|window_id| !existing_window_ids.contains(*window_id))
        {
            return Ok(window_id.clone());
        }

        thread::sleep(Duration::from_millis(GHOSTTY_DISCOVERY_DELAY_MS));
    }

    bail!("timed out waiting for the new Ghostty window")
}

fn capture_ghostty_window_frame() -> Result<WindowFrame> {
    let output = run_osascript(
        r#"tell application "System Events"
  tell process "Ghostty"
    set targetWindow to missing value
    repeat with candidate in windows
      try
        if value of attribute "AXMain" of candidate is true then
          set targetWindow to candidate
          exit repeat
        end if
      end try
    end repeat
    if targetWindow is missing value then
      if (count of windows) is 0 then error "Ghostty has no visible window"
      set targetWindow to window 1
    end if
    set {xPos, yPos} to position of targetWindow
    set {winWidth, winHeight} to size of targetWindow
    return (xPos as text) & tab & (yPos as text) & tab & (winWidth as text) & tab & (winHeight as text)
  end tell
end tell"#,
    )
    .context(
        "failed to read the current Ghostty window frame via System Events (check Accessibility permissions)",
    )?;

    parse_window_frame(&output)
}

fn parse_window_frame(raw: &str) -> Result<WindowFrame> {
    let parts: Vec<&str> = raw.trim().split('\t').map(str::trim).collect();
    if parts.len() != 4 {
        bail!("could not parse Ghostty window frame");
    }

    let x = parts[0]
        .parse::<i32>()
        .context("could not parse Ghostty window frame x position")?;
    let y = parts[1]
        .parse::<i32>()
        .context("could not parse Ghostty window frame y position")?;
    let width = parts[2]
        .parse::<i32>()
        .context("could not parse Ghostty window frame width")?;
    let height = parts[3]
        .parse::<i32>()
        .context("could not parse Ghostty window frame height")?;

    if width <= 0 || height <= 0 {
        bail!("Ghostty window frame size must be positive");
    }

    Ok(WindowFrame {
        x,
        y,
        width,
        height,
    })
}

fn parse_workspace_rows(script: &str) -> Vec<TabRow> {
    let home = home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .into_os_string()
        .into_string()
        .unwrap_or_else(|_| "/".to_string());

    parsed_workspace_tabs(script)
        .into_iter()
        .map(|tab| TabRow {
            working_dir: tab
                .working_dir
                .filter(|working_dir| !working_dir.is_empty())
                .unwrap_or_else(|| home.clone()),
            title: tab.title.unwrap_or_default(),
        })
        .collect()
}

fn plan_workspace_launch(script: &str, rows: &[TabRow]) -> WorkspaceLaunchMode {
    if workspace_requires_true_legacy_launch(script) {
        WorkspaceLaunchMode::DirectLegacy
    } else if rows.is_empty() {
        WorkspaceLaunchMode::DirectFallback
    } else {
        WorkspaceLaunchMode::DirectSplit
    }
}

fn parse_workspace_tabs(script: &str) -> Vec<WorkspaceTab> {
    parsed_workspace_tabs(script)
        .into_iter()
        .enumerate()
        .map(|(index, tab)| WorkspaceTab {
            title: match tab.title.filter(|title| !title.is_empty()) {
                Some(title) => title,
                None => fallback_tab_name(index + 1, tab.working_dir.as_deref()),
            },
            working_dir: tab
                .working_dir
                .filter(|working_dir| !working_dir.is_empty()),
        })
        .collect()
}

fn parse_workspace_layout(script: &str) -> Vec<WorkspaceTabLayout> {
    use std::collections::HashMap;

    #[derive(Clone, Debug)]
    enum Node {
        Leaf { wd: String },
        SplitRight { left: usize, right: usize },
        SplitDown { top: usize, bottom: usize },
    }

    #[derive(Clone, Debug)]
    struct TabBuilder {
        root: usize,
        root_var: String,
        title: Option<String>,
        nodes: Vec<Node>,
        var_to_leaf: HashMap<String, usize>,
    }

    fn parse_var_index(var: &str) -> Option<usize> {
        let digits_pos = var.find(|ch: char| ch.is_ascii_digit())?;
        var[digits_pos..].parse::<usize>().ok()
    }

    fn parse_root_pane_var(line: &str) -> Option<String> {
        let rest = line.strip_prefix("set ")?;
        let (var, after) = rest.split_once(" to focused terminal of ")?;
        // tab roots in gtab-generated scripts look like:
        // - selected tab of win
        // - focused terminal of newtabX / tabX
        if !after.starts_with("selected tab of win")
            && !after.starts_with("newtab")
            && !after.starts_with("tab")
        {
            return None;
        }
        Some(var.trim().to_string())
    }

    fn parse_split(line: &str) -> Option<(String, String, bool, usize)> {
        // set p<new> to split p<target> direction right|down with configuration cfg<idx>
        let rest = line.strip_prefix("set ")?;
        let (new_var, rest) = rest.split_once(" to split ")?;
        let (target_var, rest) = rest.split_once(" direction ")?;
        let (dir, rest) = rest.split_once(" with configuration cfg")?;
        let cfg_index = rest.trim().parse::<usize>().ok()?;

        let is_right = match dir.trim() {
            "right" => true,
            "down" => false,
            _ => return None,
        };

        Some((
            new_var.trim().to_string(),
            target_var.trim().to_string(),
            is_right,
            cfg_index,
        ))
    }

    fn parse_set_tab_title(line: &str) -> Option<(String, String)> {
        let rest = line.strip_prefix("perform action \"set_tab_title:")?;
        let quoted = format!("\"{rest}");
        let (title, after_title) = parse_apple_string_prefix(&quoted)?;
        let after_on = after_title.strip_prefix(" on ")?;
        let var = after_on.trim();
        Some((var.to_string(), title))
    }

    let mut cfg_wd: HashMap<usize, String> = HashMap::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if let Some((index, working_dir)) =
            parse_indexed_assignment(trimmed, "set initial working directory of cfg", " to ")
        {
            cfg_wd.insert(index, working_dir);
        }
    }

    let mut tabs: Vec<TabBuilder> = Vec::new();
    let mut current_tab: Option<usize> = None;

    for line in script.lines() {
        let trimmed = line.trim();

        if let Some(root_var) = parse_root_pane_var(trimmed) {
            let cfg_index = parse_var_index(&root_var).unwrap_or(0);
            let wd = cfg_wd.get(&cfg_index).cloned().unwrap_or_default();
            let mut nodes = Vec::new();
            nodes.push(Node::Leaf { wd });
            let mut var_to_leaf = HashMap::new();
            var_to_leaf.insert(root_var.clone(), 0);
            tabs.push(TabBuilder {
                root: 0,
                root_var,
                title: None,
                nodes,
                var_to_leaf,
            });
            current_tab = Some(tabs.len() - 1);
            continue;
        }

        if let Some(tab_index) = current_tab {
            if let Some((var, title)) = parse_set_tab_title(trimmed) {
                if var == tabs[tab_index].root_var {
                    tabs[tab_index].title = Some(title);
                }
                continue;
            }

            if let Some((new_var, target_var, is_right, cfg_index)) = parse_split(trimmed) {
                let Some(&target_leaf) = tabs[tab_index].var_to_leaf.get(&target_var) else {
                    continue;
                };

                let target_wd = match &tabs[tab_index].nodes[target_leaf] {
                    Node::Leaf { wd } => wd.clone(),
                    _ => continue,
                };
                let new_wd = cfg_wd.get(&cfg_index).cloned().unwrap_or_default();
                let left_or_top = tabs[tab_index].nodes.len();
                tabs[tab_index].nodes.push(Node::Leaf { wd: target_wd });
                let right_or_bottom = tabs[tab_index].nodes.len();
                tabs[tab_index].nodes.push(Node::Leaf { wd: new_wd });

                tabs[tab_index].nodes[target_leaf] = if is_right {
                    Node::SplitRight {
                        left: left_or_top,
                        right: right_or_bottom,
                    }
                } else {
                    Node::SplitDown {
                        top: left_or_top,
                        bottom: right_or_bottom,
                    }
                };

                // After a split, the original variable still refers to the original pane.
                tabs[tab_index].var_to_leaf.insert(target_var, left_or_top);
                tabs[tab_index].var_to_leaf.insert(new_var, right_or_bottom);
            }
        }
    }

    fn build_layout(nodes: &[Node], index: usize) -> WorkspacePaneLayout {
        match &nodes[index] {
            Node::Leaf { wd } => WorkspacePaneLayout::Leaf {
                working_dir: wd.clone(),
            },
            Node::SplitRight { left, right } => WorkspacePaneLayout::SplitRight {
                left: Box::new(build_layout(nodes, *left)),
                right: Box::new(build_layout(nodes, *right)),
            },
            Node::SplitDown { top, bottom } => WorkspacePaneLayout::SplitDown {
                top: Box::new(build_layout(nodes, *top)),
                bottom: Box::new(build_layout(nodes, *bottom)),
            },
        }
    }

    tabs.into_iter()
        .enumerate()
        .map(|(index, tab)| {
            let root_wd =
                tab.var_to_leaf
                    .get(&tab.root_var)
                    .and_then(|leaf| match &tab.nodes[*leaf] {
                        Node::Leaf { wd } => Some(wd.as_str()),
                        _ => None,
                    });
            WorkspaceTabLayout {
                title: tab
                    .title
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| fallback_tab_name(index + 1, root_wd)),
                root: build_layout(&tab.nodes, tab.root),
            }
        })
        .collect()
}

fn parsed_workspace_tabs(script: &str) -> Vec<ParsedWorkspaceTab> {
    let mut tabs: Vec<ParsedWorkspaceTab> = Vec::new();

    for line in script.lines() {
        let trimmed = line.trim();

        if let Some((index, working_dir)) =
            parse_indexed_assignment(trimmed, "set initial working directory of cfg", " to ")
        {
            ensure_tab_slot(&mut tabs, index);
            tabs[index - 1].working_dir = Some(working_dir);
            continue;
        }

        if let Some((index, title)) = parse_title_assignment(trimmed) {
            ensure_tab_slot(&mut tabs, index);
            tabs[index - 1].title = Some(title);
        }
    }

    tabs
}

fn workspace_requires_true_legacy_launch(script: &str) -> bool {
    script.lines().map(str::trim).any(|line| {
        line.starts_with("set command of cfg")
            || line.starts_with("set initial input of cfg")
            || line.starts_with("set wait after command of cfg")
            || line.starts_with("set environment variables of cfg")
    })
}

fn parse_indexed_assignment(line: &str, prefix: &str, marker: &str) -> Option<(usize, String)> {
    let rest = line.strip_prefix(prefix)?;
    let (index, rest) = split_digits(rest)?;
    let value = rest.strip_prefix(marker)?;
    parse_apple_string(value).map(|parsed| (index, parsed))
}

fn parse_title_assignment(line: &str) -> Option<(usize, String)> {
    let rest = line.strip_prefix("perform action \"set_tab_title:")?;
    let quoted = format!("\"{rest}");
    let (title, after_title) = parse_apple_string_prefix(&quoted)?;
    let after_on = after_title.strip_prefix(" on ")?;
    let after_var = after_on
        .strip_prefix("term")
        .or_else(|| after_on.strip_prefix('p'))?;
    let (index, _) = split_digits(after_var)?;
    Some((index, title))
}

fn split_digits(value: &str) -> Option<(usize, &str)> {
    let digits_len = value.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }

    let (digits, rest) = value.split_at(digits_len);
    let index = digits.parse().ok()?;
    Some((index, rest))
}

fn parse_apple_string(value: &str) -> Option<String> {
    parse_apple_string_prefix(value.trim_start()).map(|(parsed, _)| parsed)
}

fn parse_apple_string_prefix(value: &str) -> Option<(String, &str)> {
    let quoted = value.strip_prefix('"')?;
    let mut escaped = false;
    let mut out = String::new();

    for (index, ch) in quoted.char_indices() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some((out, &quoted[index + ch.len_utf8()..])),
            _ => out.push(ch),
        }
    }

    None
}

fn fallback_tab_name(index: usize, working_dir: Option<&str>) -> String {
    let Some(working_dir) = working_dir.map(str::trim).filter(|value| !value.is_empty()) else {
        return format!("Tab {index}");
    };

    if working_dir
        == home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .to_string_lossy()
    {
        return "~".to_string();
    }

    Path::new(working_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| working_dir.to_string())
}

fn apple_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn build_ghostty_shortcut_include(shortcut: &str, mode: ShortcutMode) -> String {
    if is_shortcut_disabled(shortcut) {
        return "# Managed by gtab. Update this with `gtab init` or `gtab set ghostty_shortcut`.\n# Ghostty-local shortcut is disabled.\n".to_string();
    }

    match mode {
        ShortcutMode::Text => format!(
            "# Managed by gtab. Update this with `gtab init` or `gtab set ghostty_shortcut`.\n# shortcut_mode = text: type `gtab` into the focused shell for same-tab launch.\nkeybind = {shortcut}=text:gtab\\x0d\n"
        ),
        ShortcutMode::Shell => format!(
            "# Managed by gtab. Update this with `gtab init` or `gtab set ghostty_shortcut`.\n# shortcut_mode = shell: send an invisible trigger handled by `gtab shell-init`.\n# Nothing is echoed into the prompt and nothing enters shell history.\nkeybind = {shortcut}=text:{SHELL_TRIGGER_GHOSTTY_ESCAPE}\n"
        ),
    }
}

fn render_ghostty_include_config_line(include_path: &Path) -> String {
    format!("config-file = \"{}\"", include_path.display())
}

fn sync_ghostty_include_reference(
    config_path: &Path,
    include_path: &Path,
    enabled: bool,
) -> Result<GhosttyConfigSync> {
    if enabled {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    let existing = match fs::read_to_string(config_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if enabled && ghostty_config_is_externally_managed(config_path) {
                return Ok(GhosttyConfigSync::ManualConfigRequired {
                    reason: GHOSTTY_EXTERNAL_CONFIG_REASON.to_string(),
                });
            }
            if enabled {
                let next = render_ghostty_config_with_gtab_include(&[], include_path);
                return write_ghostty_config(config_path, next);
            }
            return Ok(GhosttyConfigSync::Unchanged);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };

    let existing_lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let stripped_lines = strip_gtab_include_reference(&existing_lines, include_path);
    let next = if enabled {
        render_ghostty_config_with_gtab_include(&stripped_lines, include_path)
    } else {
        render_ghostty_config(&stripped_lines)
    };

    if next == existing {
        return Ok(GhosttyConfigSync::Unchanged);
    }

    if ghostty_config_is_externally_managed(config_path) {
        return Ok(GhosttyConfigSync::ManualConfigRequired {
            reason: GHOSTTY_EXTERNAL_CONFIG_REASON.to_string(),
        });
    }

    write_ghostty_config(config_path, next)
}

fn write_ghostty_config(config_path: &Path, next: String) -> Result<GhosttyConfigSync> {
    match fs::write(config_path, next) {
        Ok(()) => Ok(GhosttyConfigSync::Updated),
        Err(error) if is_manual_config_error(&error) => {
            Ok(GhosttyConfigSync::ManualConfigRequired {
                reason: GHOSTTY_EXTERNAL_CONFIG_REASON.to_string(),
            })
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to write {}", config_path.display()))
        }
    }
}

fn ghostty_config_is_externally_managed(config_path: &Path) -> bool {
    if config_path.starts_with(NIX_STORE_ROOT) {
        return true;
    }

    resolve_symlink_chain(config_path, 8)
        .map(|path| path.starts_with(NIX_STORE_ROOT))
        .unwrap_or(false)
}

fn resolve_symlink_chain(path: &Path, max_depth: usize) -> Result<PathBuf> {
    let mut current = path.to_path_buf();

    for _ in 0..max_depth {
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        };

        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }

        let target = fs::read_link(&current)
            .with_context(|| format!("failed to read symlink {}", current.display()))?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }

    Ok(current)
}

fn is_manual_config_error(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::PermissionDenied)
        || matches!(error.raw_os_error(), Some(30))
}

fn strip_gtab_include_reference(lines: &[String], include_path: &Path) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(lines.len());
    let mut index = 0;

    while index < lines.len() {
        if is_gtab_include_reference_line(&lines[index], include_path) {
            if kept.last().map(|line| line.trim()) == Some("# gtab managed include") {
                kept.pop();
            }
            if kept.last().is_some_and(|line| line.trim().is_empty()) {
                kept.pop();
            }
            index += 1;
            continue;
        }

        kept.push(lines[index].clone());
        index += 1;
    }

    kept
}

fn render_ghostty_config_with_gtab_include(lines: &[String], include_path: &Path) -> String {
    let mut next = render_ghostty_config(lines);

    if !next.is_empty() {
        next.push('\n');
    }

    next.push_str("# gtab managed include\n");
    next.push_str(&render_ghostty_include_config_line(include_path));
    next.push('\n');
    next
}

fn render_ghostty_config(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

fn is_gtab_include_reference_line(line: &str, include_path: &Path) -> bool {
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };

    key.trim() == "config-file"
        && value.trim().trim_matches('"') == include_path.display().to_string()
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(true)
}

#[derive(Clone, Debug, Default)]
struct ParsedWorkspaceTab {
    working_dir: Option<String>,
    title: Option<String>,
}

fn ensure_tab_slot(tabs: &mut Vec<ParsedWorkspaceTab>, index: usize) {
    while tabs.len() < index {
        tabs.push(ParsedWorkspaceTab::default());
    }
}

#[derive(Clone, Debug)]
pub struct GhosttyShortcutSync {
    pub config_path: PathBuf,
    pub include_path: PathBuf,
    pub shortcut: String,
    pub mode: ShortcutMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhosttyShortcutApplyStatus {
    UpdatedConfig,
    AlreadyConfigured,
    ManualConfigRequired,
    ManualConfigRemovalRequired,
}

#[derive(Clone, Debug)]
pub struct GhosttyShortcutApplyResult {
    pub sync: GhosttyShortcutSync,
    pub status: GhosttyShortcutApplyStatus,
    pub reason: Option<String>,
}

impl GhosttyShortcutApplyResult {
    pub fn include_config_line(&self) -> String {
        render_ghostty_include_config_line(&self.sync.include_path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GhosttyConfigSync {
    Updated,
    Unchanged,
    ManualConfigRequired { reason: String },
}

fn run_osascript(script: &str) -> Result<String> {
    let mut child = Command::new("osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to launch osascript")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("failed to open osascript stdin"))?;
        use std::io::Write as _;
        stdin
            .write_all(script.as_bytes())
            .context("failed to write AppleScript")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for osascript")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn hup_parent_process() -> Result<()> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid])
        .output()
        .context("failed to resolve parent process")?;

    if !output.status.success() {
        bail!("failed to resolve parent process");
    }

    let ppid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ppid.is_empty() {
        bail!("failed to resolve parent process");
    }

    let status = Command::new("kill")
        .args(["-HUP", &ppid])
        .status()
        .context("failed to signal parent process")?;

    if !status.success() {
        bail!("failed to signal parent process");
    }

    Ok(())
}

pub fn format_workspace_list(workspaces: &[Workspace]) -> String {
    if workspaces.is_empty() {
        return "No workspaces saved.".to_string();
    }

    let mut lines = vec!["Workspaces:".to_string()];
    for workspace in workspaces {
        lines.push(format!("  - {}", workspace.name));
    }
    lines.join("\n")
}

pub fn format_settings(env: &AppEnv) -> String {
    let close_tab = if env.config.close_tab { "on" } else { "off" };
    let ghostty = env.preview_ghostty_shortcut_sync();
    let ghostty_note = if is_shortcut_disabled(&env.config.ghostty_shortcut) {
        "Ghostty-local shortcut is disabled. Run `gtab init` to restore the default same-shell Cmd+G."
            .to_string()
    } else {
        match env.config.shortcut_mode {
            ShortcutMode::Text => "The shortcut types `gtab` into the focused Ghostty shell, so the command stays on screen. Run `gtab shell-init zsh` for a silent shortcut.".to_string(),
            ShortcutMode::Shell => {
                let shell = detect_shell_kind().map(ShellKind::as_str).unwrap_or("zsh");
                if env.shell_integration_installed() {
                    "The shortcut sends an invisible trigger handled by the `gtab shell-init` widget in your shell rc.".to_string()
                } else {
                    format!(
                        "WARNING: no `gtab shell-init` line found in your shell rc, so the shortcut will do nothing. Add `eval \"$(gtab shell-init {shell})\"`, or run `gtab set shortcut_mode text`."
                    )
                }
            }
        }
    };

    format!(
        "Settings:\n  close_tab = {close_tab}\n  ghostty_shortcut = {}\n  shortcut_mode = {}\n  ghostty_config = {}\n  ghostty_include = {}\n  {ghostty_note}",
        env.config.ghostty_shortcut,
        env.config.shortcut_mode.as_str(),
        ghostty.config_path.display(),
        ghostty.include_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AppEnv, Config, GhosttyConfigSync, GhosttyShortcutApplyStatus, ShellKind, ShortcutMode,
        TabRow, WindowFrame, WorkspaceLaunchMode, apple_escape, build_ghostty_cd_script,
        build_ghostty_replace_directory_script, build_ghostty_shortcut_include,
        build_workspace_script, format_workspace_list, looks_like_shell_default_title,
        normalize_captured_tab_title, normalize_shortcut_mode, parse_shell_kind,
        parse_window_frame, parse_workspace_rows, parse_workspace_tabs, plan_workspace_launch,
        render_ghostty_direct_cd_command, render_ghostty_include_config_line,
        render_shell_cd_command, render_shell_init, shell_integration_installed,
        should_switch_to_ascii_input_source, sync_ghostty_include_reference,
        validate_workspace_name, workspace_requires_true_legacy_launch,
    };
    use std::{fs, path::PathBuf};

    #[cfg(unix)]
    use std::os::unix::{fs::symlink, prelude::PermissionsExt};

    fn test_env(name: &str) -> AppEnv {
        let base_dir = tempfile_path(name);
        std::fs::create_dir_all(&base_dir).unwrap();
        AppEnv {
            config_file: base_dir.join("config"),
            base_dir,
            config: Config::default(),
        }
    }

    #[test]
    fn config_parses_close_tab_truthy_values() {
        let path = tempfile_path("config");
        std::fs::write(
            &path,
            "close_tab=true\nglobal_shortcut=cmd+g\nghostty_shortcut=cmd+shift+g\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.close_tab);
        assert_eq!(config.ghostty_shortcut, "cmd+shift+g");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn config_normalizes_disabled_ghostty_shortcut() {
        let path = tempfile_path("config-off");
        std::fs::write(&path, "global_shortcut=cmd+g\nghostty_shortcut=disabled\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.ghostty_shortcut, "off");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn config_defaults_to_ghostty_local_cmd_g() {
        let config = Config::default();
        assert_eq!(config.ghostty_shortcut, "cmd+g");
    }

    #[test]
    fn validate_workspace_name_rejects_empty_and_path_like_names() {
        for value in ["", "   ", ".", "..", "alpha/beta"] {
            assert!(
                validate_workspace_name(value).is_err(),
                "{value:?} should fail"
            );
        }
    }

    #[test]
    fn validate_workspace_name_accepts_plain_names() {
        for value in ["alpha", "demo-1", "hello_world"] {
            assert!(
                validate_workspace_name(value).is_ok(),
                "{value:?} should pass"
            );
        }
    }

    #[test]
    fn config_ignores_removed_launch_mode() {
        let path = tempfile_path("config-launch-mode");
        std::fs::write(
            &path,
            "global_shortcut=cmd+g\nghostty_shortcut=off\nlaunch_mode=inject\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.ghostty_shortcut, "off");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ascii_input_source_switch_skips_matching_source_ids() {
        assert!(!should_switch_to_ascii_input_source(
            Some("com.apple.keylayout.ABC"),
            Some("com.apple.keylayout.ABC"),
            false,
        ));
    }

    #[test]
    fn ascii_input_source_switch_skips_matching_source_refs() {
        assert!(!should_switch_to_ascii_input_source(None, None, true));
    }

    #[test]
    fn ascii_input_source_switch_uses_ascii_source_when_current_differs() {
        assert!(should_switch_to_ascii_input_source(
            Some("com.apple.inputmethod.SCIM.ITABC"),
            Some("com.apple.keylayout.ABC"),
            false,
        ));
    }

    #[test]
    fn apple_script_generation_preserves_workspace_structure() {
        let script = build_workspace_script(&[
            TabRow {
                working_dir: "/tmp/demo".to_string(),
                title: "main".to_string(),
            },
            TabRow {
                working_dir: "/tmp/api".to_string(),
                title: String::new(),
            },
        ]);

        assert!(script.contains("set win to new window"));
        assert!(script.contains("new tab in win"));
        assert!(script.contains("set_tab_title:main"));
    }

    #[test]
    fn apple_escape_handles_quotes_and_backslashes() {
        assert_eq!(
            apple_escape(r#"/tmp/"quote"\path"#),
            r#"/tmp/\"quote\"\\path"#
        );
    }

    #[test]
    fn workspace_preview_uses_titles_and_fallbacks() {
        let tabs = parse_workspace_tabs(
            r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set term1 to focused terminal of selected tab of win
    perform action "set_tab_title:api" on term1

    set cfg2 to new surface configuration
    set initial working directory of cfg2 to "/tmp/work"
    set tab2 to new tab in win with configuration cfg2
    set term2 to focused terminal of tab2
end tell
"#,
        );

        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title, "api");
        assert_eq!(tabs[0].working_dir.as_deref(), Some("/tmp/project"));
        assert_eq!(tabs[1].title, "work");
        assert_eq!(tabs[1].working_dir.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn workspace_preview_skips_malformed_lines_and_keeps_valid_tabs() {
        let tabs = parse_workspace_tabs(
            r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set term1 to focused terminal of selected tab of win
    perform action "set_tab_title:api" on term1
    perform action "set_tab_title:broken

    set cfg2 to new surface configuration
    set initial working directory of cfg2 to "/tmp/worker"
    set tab2 to new tab in win with configuration cfg2
    set term2 to focused terminal of tab2
end tell
"#,
        );

        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title, "api");
        assert_eq!(tabs[1].title, "worker");
    }

    #[test]
    fn workspace_rows_preserve_empty_titles_for_relaunch() {
        let rows = parse_workspace_rows(
            r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set term1 to focused terminal of selected tab of win

    set cfg2 to new surface configuration
    set initial working directory of cfg2 to "/tmp/work"
    set tab2 to new tab in win with configuration cfg2
    set term2 to focused terminal of tab2
end tell
"#,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].working_dir, "/tmp/project");
        assert!(rows[0].title.is_empty());
        assert_eq!(rows[1].working_dir, "/tmp/work");
        assert!(rows[1].title.is_empty());
    }

    #[test]
    fn captured_titles_strip_transient_prefixes() {
        assert_eq!(
            normalize_captured_tab_title("⠐ 🔔 dither-motion"),
            "dither-motion"
        );
        assert_eq!(normalize_captured_tab_title("🔔 dither"), "dither");
        assert_eq!(normalize_captured_tab_title("  api  "), "api");
    }

    #[test]
    fn shell_default_title_detection_suppresses_expected_patterns() {
        let wd = "/Users/fran/Documents/GitHub/dither-motion";
        let cases = [
            (
                "fran@frandeMacBook-Pro:~/Documents/GitHub/dither-motion",
                wd,
            ),
            (
                "~/Documents/GitHub/gtab",
                "/Users/fran/Documents/GitHub/gtab",
            ),
            (
                "…/GitHub/rss-breeze/",
                "/Users/fran/Documents/GitHub/rss-breeze",
            ),
            ("zsh", "/Users/fran"),
            ("", wd),
            ("   ", wd),
            ("🔔 fran@host:~/foo", "/tmp"),
        ];
        for (title, working_dir) in cases {
            assert!(
                looks_like_shell_default_title(title, working_dir),
                "{title:?} with wd={working_dir:?} should be treated as shell default"
            );
        }
    }

    #[test]
    fn shell_default_title_detection_preserves_custom_titles() {
        let cases = [
            ("dither-motion", "/Users/fran/Documents/GitHub/new"),
            ("rss", "/Users/fran/Documents/GitHub/rss-breeze"),
            ("api", "/tmp/project"),
            ("operation", "/Users/fran/Documents/GitHub/new"),
            ("my project", "/Users/fran/work"),
        ];
        for (title, working_dir) in cases {
            assert!(
                !looks_like_shell_default_title(title, working_dir),
                "{title:?} with wd={working_dir:?} should be preserved as custom title"
            );
        }
    }

    #[test]
    fn build_workspace_script_suppresses_shell_default_titles() {
        let script = build_workspace_script(&[
            TabRow {
                working_dir: "/tmp/project".to_string(),
                title: "fran@host:~/tmp/project".to_string(), // shell default
            },
            TabRow {
                working_dir: "/Users/fran/Documents/GitHub/new".to_string(),
                title: "operation".to_string(), // custom (not equal to basename "new")
            },
        ]);

        assert!(!script.contains("set_tab_title:fran@host"));
        assert!(script.contains("set_tab_title:operation"));
    }

    #[test]
    fn custom_workspace_features_require_legacy_launch() {
        assert!(workspace_requires_true_legacy_launch(
            r#"tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set command of cfg1 to "npm run dev"
end tell"#
        ));

        assert!(!workspace_requires_true_legacy_launch(
            r#"tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
end tell"#
        ));
    }

    #[test]
    fn launch_plan_prefers_legacy_for_custom_commands() {
        let script = r#"tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set command of cfg1 to "npm run dev"
end tell"#;

        let rows = parse_workspace_rows(script);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectLegacy
        );
    }

    #[test]
    fn launch_plan_uses_split_mode_for_split_workspaces() {
        let script = r#"tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set p1 to split p0 direction right with configuration cfg2
end tell"#;

        let rows = parse_workspace_rows(script);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectSplit
        );
    }

    #[test]
    fn launch_plan_uses_direct_split_mode_for_plain_workspaces() {
        let script = r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set term1 to focused terminal of selected tab of win
end tell"#;

        let rows = parse_workspace_rows(script);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectSplit
        );
    }

    #[test]
    fn launch_plan_falls_back_when_rows_cannot_be_reconstructed() {
        let script = r#"tell application "Ghostty"
    activate
end tell"#;

        let rows = parse_workspace_rows(script);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectFallback
        );
    }

    #[test]
    fn split_workspace_uses_direct_split_launch() {
        let script = r#"tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set p1 to split p0 direction right with configuration cfg2
end tell"#;
        let rows = parse_workspace_rows(script);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectSplit
        );

        // split workspaces are not treated as "true legacy"
        assert!(!workspace_requires_true_legacy_launch(script));
    }

    /// Regression guard for the v1.4.1–v1.4.3 TUI launch-path bug: tab-only
    /// workspaces with custom titles were launched via a multi-step path
    /// (create window → poll for new window id → reposition → separate
    /// followup script that set titles). The gap between "create window" and
    /// "set_tab_title:" gave the shell's precmd time to overwrite the title
    /// with the shell default (e.g. `fran@host:~/path`), so saved titles
    /// never survived a TUI launch.
    ///
    /// All non-legacy workspaces must route through `DirectSplit`, which runs
    /// the saved `.applescript` file in a single `osascript` invocation so
    /// that `set_tab_title:` executes in the same Apple Events queue as
    /// `new window`, matching the v1.3.x behavior that worked. Do not
    /// reintroduce a launch mode that issues `new window` and `set_tab_title:`
    /// from separate `osascript` processes.
    ///
    /// The script below mirrors the exact format produced by
    /// `capture_workspace_script`'s embedded Python generator (tab-only
    /// workspaces, `perform action "set_tab_title:..." on p<N>`). Note that
    /// `parse_workspace_rows` only recognises the `on term<N>` form used by
    /// the cfg(test) `build_workspace_script` helper, so `rows[i].title` is
    /// empty here — but that's fine: the launch planner only needs
    /// `rows.len()` to pick the mode, and `DirectSplit` hands the whole
    /// file to `osascript` which executes every `set_tab_title:` verbatim.
    #[test]
    fn tab_only_workspace_with_custom_titles_uses_direct_split_launch() {
        let script = r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/tmp/project"
    set win to new window with configuration cfg1
    set p1 to focused terminal of selected tab of win
    perform action "set_tab_title:alpha" on p1

    set cfg2 to new surface configuration
    set initial working directory of cfg2 to "/tmp/work"
    set newtab1 to new tab in win with configuration cfg2
    set p2 to focused terminal of newtab1
    perform action "set_tab_title:beta" on p2
end tell"#;

        let rows = parse_workspace_rows(script);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            plan_workspace_launch(script, &rows),
            WorkspaceLaunchMode::DirectSplit
        );
        assert!(!workspace_requires_true_legacy_launch(script));
        // Sanity: the saved script still contains the title commands so that
        // the single-osascript launch will re-apply them.
        assert!(script.contains("set_tab_title:alpha"));
        assert!(script.contains("set_tab_title:beta"));
    }

    #[test]
    fn parse_window_frame_reads_tab_separated_numbers() {
        let frame = parse_window_frame("40\t80\t1440\t900").unwrap();

        assert_eq!(
            frame,
            WindowFrame {
                x: 40,
                y: 80,
                width: 1440,
                height: 900,
            }
        );
    }

    #[test]
    fn rename_workspace_renames_applescript_file() {
        let env = test_env("rename-workspace");
        let old_path = env.base_dir.join("alpha.applescript");
        let new_path = env.base_dir.join("beta.applescript");
        std::fs::write(&old_path, "tell application \"Ghostty\"\nend tell\n").unwrap();

        let renamed = env.rename_workspace("alpha", "beta").unwrap();

        assert_eq!(renamed, new_path);
        assert!(!old_path.exists());
        assert_eq!(
            std::fs::read_to_string(&renamed).unwrap(),
            "tell application \"Ghostty\"\nend tell\n"
        );

        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn rename_workspace_rejects_existing_destination() {
        let env = test_env("rename-workspace-conflict");
        std::fs::write(env.base_dir.join("alpha.applescript"), "alpha").unwrap();
        std::fs::write(env.base_dir.join("beta.applescript"), "beta").unwrap();

        let error = env
            .rename_workspace("alpha", "beta")
            .unwrap_err()
            .to_string();

        assert!(error.contains("workspace 'beta' already exists"));
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn list_directories_sorts_case_insensitively() {
        let env = test_env("list-directories");
        let dirs_path = env.base_dir.join("dirs");
        std::fs::create_dir_all(&dirs_path).unwrap();
        std::fs::write(dirs_path.join("Zulu.path"), "/tmp/zulu").unwrap();
        std::fs::write(dirs_path.join("alpha.path"), "/tmp/alpha").unwrap();
        std::fs::write(dirs_path.join("beta.path"), "/tmp/beta").unwrap();

        let directories = env.list_directories().unwrap();

        assert_eq!(
            directories
                .iter()
                .map(|directory| directory.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "Zulu"]
        );
        assert_eq!(directories[0].path, PathBuf::from("/tmp/alpha"));
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn save_directory_writes_path_file() {
        let env = test_env("save-directory");

        let saved_path = env
            .save_directory("docs", std::path::Path::new("/tmp/docs"))
            .unwrap();

        assert_eq!(saved_path, env.base_dir.join("dirs/docs.path"));
        assert_eq!(std::fs::read_to_string(&saved_path).unwrap(), "/tmp/docs");
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn save_directory_rejects_existing_name() {
        let env = test_env("save-directory-conflict");
        std::fs::create_dir_all(env.base_dir.join("dirs")).unwrap();
        std::fs::write(env.base_dir.join("dirs/docs.path"), "/tmp/docs").unwrap();

        let error = env
            .save_directory("docs", std::path::Path::new("/tmp/other"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("directory 'docs' already exists"));
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn rename_directory_renames_path_file() {
        let env = test_env("rename-directory");
        std::fs::create_dir_all(env.base_dir.join("dirs")).unwrap();
        let old_path = env.base_dir.join("dirs/alpha.path");
        let new_path = env.base_dir.join("dirs/beta.path");
        std::fs::write(&old_path, "/tmp/alpha").unwrap();

        let renamed = env.rename_directory("alpha", "beta").unwrap();

        assert_eq!(renamed, new_path);
        assert!(!old_path.exists());
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "/tmp/alpha");
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn rename_directory_rejects_existing_destination() {
        let env = test_env("rename-directory-conflict");
        std::fs::create_dir_all(env.base_dir.join("dirs")).unwrap();
        std::fs::write(env.base_dir.join("dirs/alpha.path"), "/tmp/alpha").unwrap();
        std::fs::write(env.base_dir.join("dirs/beta.path"), "/tmp/beta").unwrap();

        let error = env
            .rename_directory("alpha", "beta")
            .unwrap_err()
            .to_string();

        assert!(error.contains("directory 'beta' already exists"));
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn remove_directory_reports_not_found() {
        let env = test_env("remove-directory-missing");

        let error = env.remove_directory("missing").unwrap_err().to_string();

        assert!(error.contains("directory 'missing' not found"));
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn validate_directory_target_rejects_missing_and_files() {
        let env = test_env("validate-directory-target");
        let missing = env.base_dir.join("missing");
        let file_path = env.base_dir.join("file.txt");
        std::fs::write(&file_path, "x").unwrap();

        assert!(env.validate_directory_target(&missing).is_err());
        let file_error = env
            .validate_directory_target(&file_path)
            .unwrap_err()
            .to_string();
        assert!(file_error.contains("is not a directory"));
        assert!(env.validate_directory_target(&env.base_dir).is_ok());
        let _ = std::fs::remove_dir_all(&env.base_dir);
    }

    #[test]
    fn render_shell_cd_command_quotes_single_quotes() {
        assert_eq!(
            render_shell_cd_command(std::path::Path::new("/tmp/it'works")),
            "cd -- '/tmp/it'\"'\"'works'"
        );
    }

    #[test]
    fn render_ghostty_direct_cd_command_prefixes_plain_cd() {
        assert_eq!(
            render_ghostty_direct_cd_command(std::path::Path::new("/tmp/it'works")),
            " cd -- '/tmp/it'\"'\"'works'"
        );
    }

    #[test]
    fn build_ghostty_cd_script_includes_input_and_enter() {
        let script = build_ghostty_cd_script("cd -- '/tmp/it'\"'\"'works'");
        assert!(script.contains("set term to focused terminal of selected tab of front window"));
        assert!(script.contains("input text \"cd -- '/tmp/it'\\\"'\\\"'works'\" to term"));
        assert!(script.contains("send key \"enter\" to term"));
    }

    #[test]
    fn build_ghostty_cd_script_keeps_no_trace_command() {
        let command = render_ghostty_direct_cd_command(std::path::Path::new("/tmp/demo"));
        let script = build_ghostty_cd_script(&command);
        let expected = format!("input text \"{}\" to term", apple_escape(&command));
        assert!(script.contains(&expected));
    }

    #[test]
    fn build_ghostty_replace_directory_script_splits_closes_and_focuses_new_surface() {
        let script = build_ghostty_replace_directory_script(std::path::Path::new("/tmp/it'works"));
        assert!(script.contains("set term to focused terminal of selected tab of front window"));
        assert!(script.contains("set cfg to new surface configuration"));
        assert!(script.contains("set initial working directory of cfg to \"/tmp/it'works\""));
        assert!(
            script.contains("set newTerm to split term direction right with configuration cfg")
        );
        assert!(script.contains("close term"));
        assert!(script.contains("focus newTerm"));
    }

    #[test]
    fn format_workspace_list_is_empty_when_no_workspaces_exist() {
        assert_eq!(format_workspace_list(&[]), "No workspaces saved.");
    }

    #[test]
    fn format_workspace_list_renders_one_name_per_line() {
        let list = format_workspace_list(&[workspace("alpha"), workspace("beta")]);

        assert_eq!(list, "Workspaces:\n  - alpha\n  - beta");
    }

    #[test]
    fn ghostty_shortcut_include_writes_keybind_command() {
        let include = build_ghostty_shortcut_include("cmd+g", ShortcutMode::Text);
        assert!(include.contains("shortcut_mode = text"));
        assert!(include.contains("keybind = cmd+g=text:gtab\\x0d"));
    }

    #[test]
    fn shell_mode_include_sends_invisible_trigger_instead_of_typing_gtab() {
        let include = build_ghostty_shortcut_include("cmd+g", ShortcutMode::Shell);
        assert!(include.contains("shortcut_mode = shell"));
        assert!(include.contains("keybind = cmd+g=text:\\x1b[71;9u"));
        assert!(!include.contains("text:gtab"));
    }

    #[test]
    fn disabled_ghostty_shortcut_include_has_no_keybind() {
        let include = build_ghostty_shortcut_include("off", ShortcutMode::Shell);
        assert!(!include.contains("keybind ="));
        assert!(include.contains("Ghostty-local shortcut is disabled"));
    }

    #[test]
    fn shortcut_mode_parses_known_values_and_rejects_others() {
        assert_eq!(
            normalize_shortcut_mode(" Shell ").unwrap(),
            ShortcutMode::Shell
        );
        assert_eq!(
            normalize_shortcut_mode("widget").unwrap(),
            ShortcutMode::Shell
        );
        assert_eq!(normalize_shortcut_mode("TEXT").unwrap(), ShortcutMode::Text);
        assert_eq!(normalize_shortcut_mode("type").unwrap(), ShortcutMode::Text);
        assert!(normalize_shortcut_mode("").is_err());
        assert!(normalize_shortcut_mode("global").is_err());
    }

    #[test]
    fn config_round_trips_shortcut_mode() {
        let path = tempfile_path("gtab-config-shortcut-mode");
        std::fs::write(
            &path,
            "close_tab=false\nghostty_shortcut=cmd+g\nshortcut_mode=shell\n",
        )
        .unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.shortcut_mode, ShortcutMode::Shell);
        assert!(config.serialize().contains("shortcut_mode=shell"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_defaults_to_text_mode_for_existing_installs() {
        let path = tempfile_path("gtab-config-legacy-mode");
        std::fs::write(&path, "close_tab=false\nghostty_shortcut=cmd+g\n").unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.shortcut_mode, ShortcutMode::Text);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shell_kind_parses_shell_paths_and_login_shell_names() {
        assert_eq!(parse_shell_kind("/bin/zsh").unwrap(), ShellKind::Zsh);
        assert_eq!(parse_shell_kind("-zsh").unwrap(), ShellKind::Zsh);
        assert_eq!(
            parse_shell_kind("/opt/homebrew/bin/fish").unwrap(),
            ShellKind::Fish
        );
        assert_eq!(parse_shell_kind("BASH").unwrap(), ShellKind::Bash);
        assert!(parse_shell_kind("  ").is_err());
        assert!(parse_shell_kind("/bin/tcsh").is_err());
    }

    #[test]
    fn shell_init_binds_the_ghostty_trigger_sequence() {
        let zsh = render_shell_init(ShellKind::Zsh);
        assert!(zsh.contains("zle -N _gtab_launch_widget"));
        assert!(zsh.contains("bindkey \'\\e[71;9u\' _gtab_launch_widget"));
        assert!(zsh.contains("zle reset-prompt"));

        let bash = render_shell_init(ShellKind::Bash);
        assert!(bash.contains("bind -m emacs-standard -x \'\"\\C-x\\C-g\": _gtab_launch_widget\'"));
        assert!(bash.contains("bind -m emacs-standard \'\"\\e[71;9u\": \"\\C-x\\C-g\"\'"));

        let fish = render_shell_init(ShellKind::Fish);
        assert!(fish.contains("bind \\e\\[71\\;9u _gtab_launch_widget"));
        assert!(fish.contains("commandline -f repaint"));
    }

    #[test]
    fn shell_init_snippets_run_gtab_without_echoing_it() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let snippet = render_shell_init(shell);
            assert!(snippet.contains("command gtab"));
        }
    }

    #[test]
    fn shell_init_never_routes_the_tui_through_the_dev_tty_clone() {
        // Regression: macOS kqueue cannot watch the /dev/tty clone device, so
        // crossterm fails with "Failed to initialize input reader" and the TUI
        // dies immediately after its first draw. Every snippet must hand gtab
        // the real terminal device instead.
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let snippet = render_shell_init(shell);
            assert!(
                !snippet.contains("</dev/tty"),
                "{shell:?} snippet still reads through the /dev/tty clone"
            );
            assert!(
                !snippet.contains(">/dev/tty "),
                "{shell:?} snippet still writes through the /dev/tty clone"
            );
        }

        // zsh detaches stdin inside zle, so it must resolve the device from $TTY.
        assert!(render_shell_init(ShellKind::Zsh).contains("${TTY:-"));
        // bash and fish keep the real tty readline/fish handed them.
        assert!(render_shell_init(ShellKind::Bash).contains("gtab_tty=$(command tty"));
        assert!(render_shell_init(ShellKind::Fish).contains("set -l gtab_tty (command tty"));
    }

    #[test]
    fn shell_integration_detection_ignores_commented_lines() {
        let home = tempfile_path("gtab-shell-rc-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".zshrc"), "# eval \"$(gtab shell-init zsh)\"\n").unwrap();
        assert!(!shell_integration_installed(&home));

        std::fs::write(home.join(".zshrc"), "eval \"$(gtab shell-init zsh)\"\n").unwrap();
        assert!(shell_integration_installed(&home));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn enabling_ghostty_sync_adds_managed_include_reference() {
        let config_path = tempfile_path("ghostty-config-enable");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");

        let sync = sync_ghostty_include_reference(&config_path, include_path, true).unwrap();

        assert_eq!(sync, GhosttyConfigSync::Updated);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "# gtab managed include\nconfig-file = \"/tmp/gtab-shortcut.conf\"\n"
        );
    }

    #[test]
    fn enabling_ghostty_sync_is_idempotent() {
        let config_path = tempfile_path("ghostty-config-idempotent");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");

        sync_ghostty_include_reference(&config_path, include_path, true).unwrap();
        let sync = sync_ghostty_include_reference(&config_path, include_path, true).unwrap();

        assert_eq!(sync, GhosttyConfigSync::Unchanged);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "# gtab managed include\nconfig-file = \"/tmp/gtab-shortcut.conf\"\n"
        );
    }

    #[test]
    fn enabling_ghostty_sync_deduplicates_existing_managed_reference() {
        let config_path = tempfile_path("ghostty-config-dedupe");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");
        std::fs::write(
            &config_path,
            concat!(
                "font-size = 15\n\n",
                "# gtab managed include\n",
                "config-file = \"/tmp/gtab-shortcut.conf\"\n\n",
                "# gtab managed include\n",
                "config-file = \"/tmp/gtab-shortcut.conf\"\n"
            ),
        )
        .unwrap();

        let sync = sync_ghostty_include_reference(&config_path, include_path, true).unwrap();

        assert_eq!(sync, GhosttyConfigSync::Updated);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            concat!(
                "font-size = 15\n\n",
                "# gtab managed include\n",
                "config-file = \"/tmp/gtab-shortcut.conf\"\n"
            )
        );
    }

    #[test]
    fn disabling_ghostty_sync_removes_managed_include_and_preserves_other_config() {
        let config_path = tempfile_path("ghostty-config-disable");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");
        std::fs::write(
            &config_path,
            concat!(
                "theme = dark\n",
                "config-file = \"/tmp/shared.conf\"\n\n",
                "# gtab managed include\n",
                "config-file = \"/tmp/gtab-shortcut.conf\"\n",
                "shell-integration = zsh"
            ),
        )
        .unwrap();

        let sync = sync_ghostty_include_reference(&config_path, include_path, false).unwrap();

        assert_eq!(sync, GhosttyConfigSync::Updated);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            concat!(
                "theme = dark\n",
                "config-file = \"/tmp/shared.conf\"\n",
                "shell-integration = zsh\n"
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn enabling_ghostty_sync_updates_writable_symlink_target() {
        let dir = tempdir_path("ghostty-config-symlink");
        fs::create_dir_all(&dir).unwrap();
        let target_path = dir.join("config");
        let link_path = dir.join("config.ghostty");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");
        fs::write(&target_path, "font-size = 14\n").unwrap();
        symlink(&target_path, &link_path).unwrap();

        let sync = sync_ghostty_include_reference(&link_path, include_path, true).unwrap();

        assert_eq!(sync, GhosttyConfigSync::Updated);
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "font-size = 14\n\n# gtab managed include\nconfig-file = \"/tmp/gtab-shortcut.conf\"\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn enabling_ghostty_sync_reports_manual_config_for_nix_symlink_chain() {
        let dir = tempdir_path("ghostty-config-nix");
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config");
        let config_ghostty_path = dir.join("config.ghostty");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");
        symlink(
            "/nix/store/gtab-test-home-manager/.config/ghostty/config",
            &config_path,
        )
        .unwrap();
        symlink("config", &config_ghostty_path).unwrap();

        let sync =
            sync_ghostty_include_reference(&config_ghostty_path, include_path, true).unwrap();

        assert_eq!(
            sync,
            GhosttyConfigSync::ManualConfigRequired {
                reason: super::GHOSTTY_EXTERNAL_CONFIG_REASON.to_string(),
            }
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn enabling_ghostty_sync_falls_back_to_manual_config_for_read_only_file() {
        let config_path = tempfile_path("ghostty-config-read-only");
        let include_path = std::path::Path::new("/tmp/gtab-shortcut.conf");
        fs::write(&config_path, "font-size = 14\n").unwrap();
        let mut permissions = fs::metadata(&config_path).unwrap().permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(&config_path, permissions).unwrap();

        let sync = sync_ghostty_include_reference(&config_path, include_path, true).unwrap();

        assert_eq!(
            sync,
            GhosttyConfigSync::ManualConfigRequired {
                reason: super::GHOSTTY_EXTERNAL_CONFIG_REASON.to_string(),
            }
        );

        let mut permissions = fs::metadata(&config_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&config_path, permissions).unwrap();
        let _ = fs::remove_file(&config_path);
    }

    #[cfg(unix)]
    #[test]
    fn init_shortcuts_reports_manual_setup_when_ghostty_config_is_nix_managed() {
        let env = test_env("init-shortcuts-nix");
        let xdg_dir = tempdir_path("xdg-dir-nix");
        let ghostty_dir = xdg_dir.join("ghostty");
        fs::create_dir_all(&ghostty_dir).unwrap();
        let config_path = ghostty_dir.join("config");
        let config_ghostty_path = ghostty_dir.join("config.ghostty");
        symlink(
            "/nix/store/gtab-test-home-manager/.config/ghostty/config",
            &config_path,
        )
        .unwrap();
        symlink("config", &config_ghostty_path).unwrap();

        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("HOME", &xdg_dir);
            std::env::set_var("XDG_CONFIG_HOME", &xdg_dir);
        }

        let mut env = env;
        let result = env.init_shortcuts().unwrap();

        assert_eq!(
            result.status,
            GhosttyShortcutApplyStatus::ManualConfigRequired
        );
        assert_eq!(
            result.include_config_line(),
            render_ghostty_include_config_line(&result.sync.include_path)
        );
        assert!(result.sync.include_path.exists());
        assert_eq!(result.sync.config_path, config_ghostty_path);

        restore_env_var("HOME", old_home);
        restore_env_var("XDG_CONFIG_HOME", old_xdg);
        let _ = fs::remove_dir_all(&env.base_dir);
        let _ = fs::remove_dir_all(&xdg_dir);
    }

    fn tempfile_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gtab-{name}-{nanos}.tmp"))
    }

    fn tempdir_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gtab-{name}-{nanos}"))
    }

    fn workspace(name: &str) -> super::Workspace {
        super::Workspace {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{name}.applescript")),
            tabs: vec![],
            layout: vec![],
        }
    }

    fn restore_env_var(key: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
