use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct TestContext {
    root: PathBuf,
    gtab_dir: PathBuf,
    home_dir: PathBuf,
    xdg_config_home: PathBuf,
}

pub struct CmdResult {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl TestContext {
    pub fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("gtab-cli-tests-{name}-{unique}"));
        let gtab_dir = root.join("gtab");
        let home_dir = root.join("home");
        let xdg_config_home = root.join("xdg");

        fs::create_dir_all(&gtab_dir).unwrap();
        fs::create_dir_all(&home_dir).unwrap();
        fs::create_dir_all(&xdg_config_home).unwrap();

        Self {
            root,
            gtab_dir,
            home_dir,
            xdg_config_home,
        }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gtab"));
        command.env_clear();
        command.env("GTAB_DIR", &self.gtab_dir);
        command.env("HOME", &self.home_dir);
        command.env("XDG_CONFIG_HOME", &self.xdg_config_home);
        command.env("NO_COLOR", "1");
        command.env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
        );
        command
    }

    pub fn run<I, S>(&self, args: I) -> CmdResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command();
        command.args(args);
        self.capture(&mut command)
    }

    pub fn capture(&self, command: &mut Command) -> CmdResult {
        let output = command.output().unwrap();
        CmdResult {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    pub fn workspace_path(&self, name: &str) -> PathBuf {
        self.gtab_dir.join(format!("{name}.applescript"))
    }

    pub fn config_path(&self) -> PathBuf {
        self.gtab_dir.join("config")
    }

    pub fn home_path(&self) -> PathBuf {
        self.home_dir.clone()
    }

    pub fn shortcut_include_path(&self) -> PathBuf {
        self.gtab_dir.join("ghostty-shortcut.conf")
    }

    pub fn ghostty_config_path(&self) -> PathBuf {
        self.xdg_config_home.join("ghostty").join("config.ghostty")
    }

    pub fn write_workspace(&self, name: &str, contents: &str) {
        fs::write(self.workspace_path(name), contents).unwrap();
    }

    pub fn read_to_string(&self, path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).unwrap()
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
