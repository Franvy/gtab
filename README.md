# gtab

`gtab` is a lightweight workspace manager for [Ghostty](https://ghostty.org) on macOS.

Save your current Ghostty window layout as a named workspace. Reopen it later with a single keystroke. That is the whole idea.

<video src="https://github.com/user-attachments/assets/beb81b3f-b28f-4b4e-a9d9-21c546a87e0a" autoplay loop muted playsinline></video>

---

## Quick Install

```bash
brew tap Franvy/gtab
brew install gtab
gtab init
```

For a shortcut that leaves no text behind, add the shell integration and re-run `gtab init`:

```bash
echo 'eval "$(gtab shell-init zsh)"' >> ~/.zshrc   # bash: shell-init bash >> ~/.bashrc
exec zsh
gtab init
```

Reload Ghostty config (or restart Ghostty), then press **Cmd+G** inside any Ghostty shell to open the workspace launcher.

---

## What It Does

- Save a Ghostty window as a named workspace — tabs, working directories, titles, and split panes
- Reopen any workspace later as a fresh Ghostty window with native tabs
- Save named directory entries and reopen the current split as a fresh shell in that directory
- Launch from a small keyboard-first TUI, or directly from the shell
- New window automatically aligns to your current Ghostty window position and size
- Rename, delete, and search workspaces without leaving the TUI
- The TUI adapts to the terminal width: quick settings hides first, then the layout preview moves below the workspace list and the names wrap into columns
- Fast in-app shortcut via `Cmd+G` set up with `gtab init` — with the shell integration installed it launches silently, leaving no command text and no shell history entry

## What It Does Not Do

- Does not persist running processes
- Does not restore shell history, editor buffers, SSH sessions, or pane state
- Does not replace tmux for detach/attach, panes, or remote workflows

---

## Typical Workflow

1. Open Ghostty, arrange your tabs the way you want.
2. Save the layout:

```bash
gtab save myproject
```

3. Press `Cmd+G` inside Ghostty (or run `gtab`) to open the TUI.
4. Type to search, press `Enter` to launch.
5. Or launch directly by name:

```bash
gtab myproject
```

---

## TUI Keys

| Key | Action |
|-----|--------|
| `f` | Toggle Workspace Space / Directory Space |
| `/` | Search current space |
| `↑` / `↓` | Move selection |
| `Enter` | Workspace: launch selected workspace; Directory: replace the current split with a fresh shell in that directory |
| `a` | Workspace: save current Ghostty window; Directory: save current shell directory |
| `n` | Rename selected item in current space |
| `d` | Delete selected item in current space |
| `e` | Workspace only: open workspace file in `$EDITOR` |
| `g` | Workspace only: edit Ghostty shortcut |
| `q` / `Esc` | Quit |

> **Double-click** also runs the primary action of the current space (launch/replace).

When you launch from the TUI, the new Ghostty window is repositioned to match your current window's position and size. This uses macOS Accessibility (System Events), so you may need to grant permission once.

---

## Directory Space

Directory Space stores named directory paths only. It does not rebuild Ghostty tabs or windows.

- Press `f` in the TUI to switch to Directory Space.
- Saved directories are shown in a grid that fills the pane width (up to 8 columns); each column is sized to its own widest entry, so short names are not padded out to the longest one.
- Press `a` to save the current shell directory as a named entry.
- Press `Enter` (or double-click) to replace the current split with a fresh shell started in that directory.

By default, gtab swaps the current split process for a new shell started in the selected directory. This keeps Directory Space zero-setup: upgrade gtab and use it immediately.

This replaces the shell process in that split, so in-flight shell state inside the old split is discarded.

If you prefer a shell-wrapper fallback (for example, running outside Ghostty), you can still use:

```bash
gtab() {
  if [ "$#" -eq 0 ]; then
    local cmd
    cmd="$(command gtab --shell-cd)" || return $?
    if [ -n "$cmd" ]; then
      eval "$cmd"
    fi
    return 0
  fi

  command gtab "$@"
}
```

`gtab --shell-cd` is only for this wrapper flow. Other commands and workspace launches are unchanged.

---

## Core Commands

```text
gtab                     Open the TUI
gtab init                Enable the Ghostty-local Cmd+G shortcut
gtab shell-init <shell>  Print the shell integration for zsh, bash, or fish
gtab save <name>         Save the current Ghostty window
gtab <name>              Launch a workspace directly
gtab list                List saved workspaces
gtab rename <old> <new>  Rename a workspace
gtab remove <name>       Remove a workspace
```

## Advanced Commands

```text
gtab edit <name>                       Open workspace file in $EDITOR
gtab set                               Show current settings
gtab set close_tab on|off              Auto-close the launching tab after launch
gtab set ghostty_shortcut cmd+g|off    Change or disable the Ghostty shortcut
gtab set shortcut_mode shell|text      Silent widget trigger, or type `gtab` into the shell
```

Workspaces are stored as plain `.applescript` files in `~/.config/gtab/`.
Directory entries are stored as plain `.path` files in `~/.config/gtab/dirs/`.

---

## Install

### Homebrew (recommended)

```bash
brew tap Franvy/gtab
brew install gtab
gtab init
```

Reload Ghostty config or restart Ghostty. Then press `Cmd+G` inside any Ghostty shell.

### Build from source

Requirements: macOS, [Ghostty](https://ghostty.org), Rust toolchain.

```bash
cargo install --path .
gtab init
```

### Update

```bash
brew upgrade gtab
```

---

## Uninstall

```bash
# Disable the Ghostty shortcut first
gtab set ghostty_shortcut off

# Reload Ghostty config so Cmd+G stops working

# Remove the shell integration line from your shell rc if you added one
#   eval "$(gtab shell-init zsh)"

# Then remove the binary
brew uninstall gtab
# or: cargo uninstall gtab

# Optionally remove saved workspaces and config
rm -rf ~/.config/gtab
```

---

## Shortcut Model

`gtab init` writes a managed Ghostty keybind file and adds an `include` line to your Ghostty config. Ghostty has no keybind action that runs an external command, so the keybind sends text to the focused shell. There are two ways to do that, selected by `shortcut_mode`.

### `shortcut_mode = shell` (recommended)

```conf
keybind = cmd+g=text:\x1b[71;9u
```

The keybind sends an invisible trigger sequence instead of a command. A shell widget installed by `gtab shell-init` is bound to that sequence and runs `gtab` directly:

```bash
eval "$(gtab shell-init zsh)"      # ~/.zshrc
eval "$(gtab shell-init bash)"     # ~/.bashrc
gtab shell-init fish | source      # ~/.config/fish/config.fish
```

Nothing is echoed into the prompt, nothing enters shell history, and a half-typed command line is restored exactly as it was when the TUI exits.

`gtab init` switches to this mode automatically once it finds a `gtab shell-init` line in your shell rc files. You can also set it explicitly with `gtab set shortcut_mode shell`. If shell mode is active but the integration is missing, `gtab init` and `gtab set` both warn that the shortcut will do nothing.

Upgrading from an older gtab changes nothing on its own: existing installs keep `shortcut_mode = text` until you install the integration and re-run `gtab init`.

### `shortcut_mode = text` (default, zero setup)

```conf
keybind = cmd+g=text:gtab\x0d
```

Ghostty types `gtab` into the active shell. No shell configuration needed, but the command stays visible on screen and lands in shell history.

**Tradeoff (both modes):** the shortcut only reaches the shell, so it is not safe inside full-screen interactive programs like Claude Code, vim, or fzf — `text` mode types `gtab` into them and `shell` mode sends a stray escape sequence. Quit those programs first, or use `gtab <name>` from a clean shell prompt.

If your Ghostty config is managed by Nix/Home Manager or another read-only setup, `gtab init` will still write `~/.config/gtab/ghostty-shortcut.conf`, then tell you to add this line to your Ghostty config source manually:

```conf
config-file = "/Users/you/.config/gtab/ghostty-shortcut.conf"
```

After that, rebuild/apply your config and reload or restart Ghostty.

---

## gtab vs tmux

| Topic | gtab | tmux |
|-------|------|------|
| Main idea | Save and relaunch Ghostty tab layouts | Full terminal multiplexer |
| Interface | Native Ghostty tabs | tmux sessions, windows, panes |
| State restored | Tab order, working dirs, titles, splits | Multiplexer-managed sessions and panes |
| Learning curve | Low | Higher |
| Remote / detach / attach | No | Yes |
| Best for | Ghostty-first macOS users | Users who need a full workflow layer |

---

## How It Works

`gtab save` reads the current Ghostty window through Ghostty's AppleScript API. For split-pane tabs, it also queries macOS Accessibility to capture pane positions, then reconstructs the split tree. The result is a plain `.applescript` file stored in `~/.config/gtab/`.

Launching a workspace runs that script via `osascript` to open a fresh Ghostty window and recreate the saved layout.

That is why `gtab` is lightweight: it stores layout metadata, not live terminal session state.

---

## FAQ

### Why does `Cmd+G` send text instead of running the binary directly?

Ghostty keybindings do not have an action for running external commands. The `text` action sends a string to the active shell, so that string is the only channel available.

`shortcut_mode = shell` uses that channel for an invisible trigger sequence rather than the literal word `gtab`, and lets a shell widget do the launching — the same technique fzf and atuin use for their key bindings.

See: [ghostty.org/docs/config/keybind](https://ghostty.org/docs/config/keybind)

### The shortcut does nothing after switching to `shortcut_mode = shell`

The widget is missing. Run `gtab set` — in shell mode it checks your shell rc files and warns if no `gtab shell-init` line is there. Add the line back and start a new shell, or fall back with `gtab set shortcut_mode text`.

The check scans every common rc file without knowing which shell you are in, so if you use both zsh and bash and only installed the integration for one of them, the shortcut works only in that shell.

### Why doesn't gtab edit my Nix/Home Manager config directly?

Nix/Home Manager usually generates Ghostty config from a declaration source instead of a normal writable file. `gtab` can safely generate its own managed include file, but it cannot reliably edit every user's `home.nix`, flake module, or repo layout without risking a bad config change. In those setups, `gtab init` writes the managed include file and tells you exactly which `config-file = ...` line to add to your config source.

### Does gtab support split panes?

Yes, as of v1.4.1. `gtab save` captures split pane layouts. Splits are restored when launching.

### My panes were evenly sized, but they come back uneven

Fixed as of v1.8.1. Ghostty creates every split at 50/50, so three evenly sized panes used to reopen as 50/25/25. `gtab save` now compares the captured pane geometry against Ghostty's `equalize_splits` result and, when they match, records that action in the workspace script so even layouts reopen even.

Uneven layouts you arranged on purpose are still restored at Ghostty's default 50/50 per split. Ghostty's AppleScript API can only create splits, not set their exact ratios, so arbitrary proportions are not preserved yet.

---

## License

MIT
