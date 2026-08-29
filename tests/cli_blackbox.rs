mod support;

use support::TestContext;

fn sample_workspace_script(title: &str, working_dir: &str) -> String {
    format!(
        r#"tell application "Ghostty"
    activate

    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "{working_dir}"
    set win to new window with configuration cfg1
    set term1 to focused terminal of selected tab of win
    perform action "set_tab_title:{title}" on term1
end tell
"#
    )
}

#[test]
fn version_prints_package_version() {
    let ctx = TestContext::new("version");

    let result = ctx.run(["--version"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout.trim(),
        format!("gtab {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn help_renders_usage() {
    let ctx = TestContext::new("help");

    let result = ctx.run(["--help"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("Ghostty Tab Workspace Manager"));
    assert!(
        result
            .stdout
            .contains("Save the current Ghostty window as a workspace")
    );
    assert!(!result.stdout.contains("--shell-cd"));
}

#[test]
fn list_reports_empty_state() {
    let ctx = TestContext::new("list-empty");

    let result = ctx.run(["list"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "No workspaces saved.\n");
}

#[test]
fn list_sorts_workspaces_case_insensitively() {
    let ctx = TestContext::new("list-sorted");
    ctx.write_workspace("Zulu", &sample_workspace_script("Zulu", "/tmp/zulu"));
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));
    ctx.write_workspace("beta", &sample_workspace_script("beta", "/tmp/beta"));

    let result = ctx.run(["list"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "Workspaces:\n  - alpha\n  - beta\n  - Zulu\n"
    );
}

#[test]
fn rename_moves_workspace_file() {
    let ctx = TestContext::new("rename");
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));

    let result = ctx.run(["rename", "alpha", "beta"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(
        result
            .stdout
            .contains("Renamed workspace \"alpha\" to \"beta\"")
    );
    assert!(!ctx.workspace_path("alpha").exists());
    assert!(ctx.workspace_path("beta").exists());
}

#[test]
fn rename_same_name_is_a_noop() {
    let ctx = TestContext::new("rename-same");
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));

    let result = ctx.run(["rename", "alpha", "alpha"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "Workspace name unchanged.\n");
    assert!(ctx.workspace_path("alpha").exists());
}

#[test]
fn remove_deletes_workspace_file() {
    let ctx = TestContext::new("remove");
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));

    let result = ctx.run(["remove", "alpha"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("Removed workspace \"alpha\""));
    assert!(!ctx.workspace_path("alpha").exists());
}

#[test]
fn remove_reports_not_found() {
    let ctx = TestContext::new("remove-missing");

    let result = ctx.run(["remove", "missing"]);

    assert!(!result.status.success());
    assert!(result.stderr.contains("workspace 'missing' not found"));
}

#[test]
fn set_close_tab_persists_and_is_reported() {
    let ctx = TestContext::new("set-close-tab");

    let set_result = ctx.run(["set", "close_tab", "on"]);
    assert!(set_result.status.success(), "stderr: {}", set_result.stderr);
    assert_eq!(set_result.stdout, "Set close_tab = on\n");

    let show_result = ctx.run(["set"]);
    assert!(
        show_result.status.success(),
        "stderr: {}",
        show_result.stderr
    );
    assert!(show_result.stdout.contains("close_tab = on"));
    assert!(
        ctx.read_to_string(ctx.config_path())
            .contains("close_tab=true")
    );
}

#[test]
fn set_close_tab_rejects_invalid_values() {
    let ctx = TestContext::new("set-close-tab-invalid");

    let result = ctx.run(["set", "close_tab", "maybe"]);

    assert!(!result.status.success());
    assert!(
        result
            .stderr
            .contains("close_tab value must be 'on' or 'off'")
    );
}

#[test]
fn set_unknown_setting_fails() {
    let ctx = TestContext::new("set-unknown");

    let result = ctx.run(["set", "mystery", "value"]);

    assert!(!result.status.success());
    assert!(result.stderr.contains("unknown setting"));
}

#[test]
fn edit_uses_editor_command_with_isolated_workspace_file() {
    let ctx = TestContext::new("edit");
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));

    let mut command = ctx.command();
    command.args(["edit", "alpha"]);
    command.env("EDITOR", "/usr/bin/true");
    let result = ctx.capture(&mut command);

    assert!(result.status.success(), "stderr: {}", result.stderr);
}

#[test]
fn init_writes_managed_shortcut_files() {
    let ctx = TestContext::new("init");

    let result = ctx.run(["init"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(
        result
            .stdout
            .contains("Initialized Ghostty-local shortcut.")
    );
    assert!(ctx.config_path().exists());
    assert!(
        ctx.workspace_path("ghostty-shortcut")
            .with_extension("conf")
            .exists()
    );
    assert!(ctx.ghostty_config_path().exists());
}

#[test]
fn init_defaults_to_text_mode_and_points_at_shell_integration() {
    let ctx = TestContext::new("init-text-mode");

    let result = ctx.run(["init"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("shortcut_mode = text"));
    assert!(result.stdout.contains("gtab shell-init"));
    assert!(
        ctx.read_to_string(ctx.shortcut_include_path())
            .contains("keybind = cmd+g=text:gtab\\x0d")
    );
}

#[test]
fn init_switches_to_shell_mode_when_the_shell_integration_is_installed() {
    let ctx = TestContext::new("init-shell-mode");
    std::fs::write(
        ctx.home_path().join(".zshrc"),
        "eval \"$(gtab shell-init zsh)\"\n",
    )
    .unwrap();

    let result = ctx.run(["init"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("shortcut_mode = shell"));
    assert!(!result.stdout.contains("Cmd+G currently types `gtab`"));

    let include = ctx.read_to_string(ctx.shortcut_include_path());
    assert!(include.contains("keybind = cmd+g=text:\\x1b[71;9u"));
    assert!(!include.contains("text:gtab"));
    assert!(
        ctx.read_to_string(ctx.config_path())
            .contains("shortcut_mode=shell")
    );
}

#[test]
fn set_shortcut_mode_rewrites_the_managed_keybind() {
    let ctx = TestContext::new("set-shortcut-mode");
    assert!(ctx.run(["init"]).status.success());

    let result = ctx.run(["set", "shortcut_mode", "shell"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("Set shortcut_mode = shell"));
    assert!(result.stdout.contains("invisible trigger"));
    assert!(result.stdout.contains("No `gtab shell-init` line found"));
    assert!(
        ctx.read_to_string(ctx.shortcut_include_path())
            .contains("keybind = cmd+g=text:\\x1b[71;9u")
    );

    let back = ctx.run(["set", "shortcut_mode", "text"]);
    assert!(back.status.success(), "stderr: {}", back.stderr);
    assert!(
        ctx.read_to_string(ctx.shortcut_include_path())
            .contains("keybind = cmd+g=text:gtab\\x0d")
    );

    let settings = ctx.run(["set"]);
    assert!(settings.stdout.contains("shortcut_mode = text"));
}

#[test]
fn init_warns_when_shell_mode_has_no_shell_integration() {
    let ctx = TestContext::new("init-shell-mode-missing-integration");
    std::fs::write(
        ctx.home_path().join(".zshrc"),
        "eval \"$(gtab shell-init zsh)\"\n",
    )
    .unwrap();
    assert!(ctx.run(["init"]).stdout.contains("shortcut_mode = shell"));

    // The user later drops the integration line but keeps shell mode.
    std::fs::remove_file(ctx.home_path().join(".zshrc")).unwrap();
    let result = ctx.run(["init"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("shortcut_mode = shell"));
    assert!(
        result
            .stdout
            .contains("no `gtab shell-init` line was found in your shell rc")
    );
    assert!(result.stdout.contains("Cmd+G will do nothing"));
    assert!(result.stdout.contains("gtab set shortcut_mode text"));

    let settings = ctx.run(["set"]);
    assert!(
        settings
            .stdout
            .contains("WARNING: no `gtab shell-init` line")
    );
}

#[test]
fn settings_confirm_a_working_shell_integration() {
    let ctx = TestContext::new("settings-shell-integration-ok");
    std::fs::write(
        ctx.home_path().join(".zshrc"),
        "eval \"$(gtab shell-init zsh)\"\n",
    )
    .unwrap();
    assert!(ctx.run(["init"]).status.success());

    let settings = ctx.run(["set"]);

    assert!(settings.stdout.contains("shortcut_mode = shell"));
    assert!(settings.stdout.contains("widget in your shell rc"));
    assert!(!settings.stdout.contains("WARNING"));
}

#[test]
fn set_shortcut_mode_rejects_invalid_values() {
    let ctx = TestContext::new("set-shortcut-mode-invalid");

    let result = ctx.run(["set", "shortcut_mode", "global"]);

    assert!(!result.status.success());
    assert!(
        result
            .stderr
            .contains("shortcut_mode value must be \'shell\' or \'text\'")
    );
}

#[test]
fn shell_init_prints_a_bindable_widget_for_each_supported_shell() {
    let ctx = TestContext::new("shell-init");

    let zsh = ctx.run(["shell-init", "zsh"]);
    assert!(zsh.status.success(), "stderr: {}", zsh.stderr);
    assert!(zsh.stdout.contains("zle -N _gtab_launch_widget"));
    assert!(
        zsh.stdout
            .contains("bindkey \'\\e[71;9u\' _gtab_launch_widget")
    );
    assert!(!zsh.stdout.contains("alias"));

    let bash = ctx.run(["shell-init", "bash"]);
    assert!(bash.status.success(), "stderr: {}", bash.stderr);
    assert!(
        bash.stdout
            .contains(r#"-x '"\C-x\C-g": _gtab_launch_widget'"#)
    );
    assert!(bash.stdout.contains(r#"'"\e[71;9u": "\C-x\C-g"'"#));

    let fish = ctx.run(["shell-init", "fish"]);
    assert!(fish.status.success(), "stderr: {}", fish.stderr);
    assert!(fish.stdout.contains("commandline -f repaint"));
}

#[test]
fn shell_init_falls_back_to_the_shell_environment_variable() {
    let ctx = TestContext::new("shell-init-env");

    let mut command = ctx.command();
    command.arg("shell-init");
    command.env("SHELL", "/opt/homebrew/bin/fish");
    let result = ctx.capture(&mut command);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert!(result.stdout.contains("gtab shell integration (fish)"));
}

#[test]
fn shell_init_rejects_unsupported_shells() {
    let ctx = TestContext::new("shell-init-unsupported");

    let result = ctx.run(["shell-init", "tcsh"]);

    assert!(!result.status.success());
    assert!(result.stderr.contains("unsupported shell \'tcsh\'"));
}

#[test]
fn shell_init_reports_a_missing_shell_environment() {
    let ctx = TestContext::new("shell-init-missing-env");

    let result = ctx.run(["shell-init"]);

    assert!(!result.status.success());
    assert!(result.stderr.contains("could not detect the current shell"));
}

#[test]
fn direct_launch_of_missing_workspace_reports_error() {
    let ctx = TestContext::new("launch-missing");

    let result = ctx.run(["missing"]);

    assert!(!result.status.success());
    assert!(result.stdout.contains("Launching \"missing\"..."));
    assert!(result.stderr.contains("workspace 'missing' not found"));
}

#[test]
fn shell_cd_flag_keeps_non_tui_list_output_unchanged() {
    let ctx = TestContext::new("shell-cd-list");
    ctx.write_workspace("alpha", &sample_workspace_script("alpha", "/tmp/alpha"));

    let result = ctx.run(["--shell-cd", "list"]);

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "Workspaces:\n  - alpha\n");
}

#[test]
fn shell_cd_flag_keeps_non_tui_error_path_unchanged() {
    let ctx = TestContext::new("shell-cd-launch-missing");

    let result = ctx.run(["--shell-cd", "missing"]);

    assert!(!result.status.success());
    assert!(result.stdout.contains("Launching \"missing\"..."));
    assert!(result.stderr.contains("workspace 'missing' not found"));
}
