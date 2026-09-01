# Gtab

[![Release](https://img.shields.io/github/v/release/Franvy/gtab)](https://github.com/Franvy/gtab/releases)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey)](https://ghostty.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`gtab` is a lightweight workspace manager for [Ghostty](https://ghostty.org) on macOS.

Save your current Ghostty window layout as a named workspace. Reopen it later with a single keystroke. That is the whole idea.

<video src="https://github.com/user-attachments/assets/104aebe0-c1ba-41f7-9625-ada7d40f6733" autoplay loop muted playsinline></video>

**Requirements:** macOS · [Ghostty](https://ghostty.org) · zsh, bash, or fish (for the silent shortcut)

---

## Install

```bash
brew tap Franvy/gtab
brew install gtab
gtab init
```

`gtab init` writes a managed Ghostty keybind file and includes it from your Ghostty config. Reload the config with **Cmd+Shift+,** (or restart Ghostty), then press **Cmd+G** inside any Ghostty shell to open the launcher.

The first time you save or launch a workspace, macOS shows a permission prompt: `gtab` drives Ghostty through System Events to read split-pane geometry and to position the new window. Allow it once. If you dismissed the prompt, re-enable it under System Settings → Privacy & Security → Accessibility.

<details>
<summary>Build from source</summary>

Requires the Rust toolchain.

```bash
git clone https://github.com/Franvy/gtab.git
cd gtab
cargo install --path .
gtab init
```

</details>

Update with `brew upgrade gtab`.

### Optional: silent shortcut

By default `Cmd+G` types `gtab` into your shell, so the command stays on screen and lands in shell history. Add the shell integration for your shell to make it invisible:

```bash
# zsh
echo 'eval "$(gtab shell-init zsh)"' >> ~/.zshrc

# bash
echo 'eval "$(gtab shell-init bash)"' >> ~/.bashrc

# fish
echo 'gtab shell-init fish | source' >> ~/.config/fish/config.fish
```

Then start a new shell and re-run `gtab init`:

```bash
exec $SHELL
gtab init
```

`gtab init` detects the integration and switches `shortcut_mode` to `shell` on its own. See [Shortcut model](#shortcut-model) for the details.

---

## Quick start

1. Open Ghostty and arrange your tabs and splits the way you want them.
2. Save the layout:

   ```bash
   gtab save myproject
   ```

3. Press `Cmd+G` (or run `gtab`) to open the TUI, type to search, press `Enter` to launch.
4. Or skip the TUI entirely:

   ```bash
   gtab myproject
   ```

---

## What it does

- Saves a Ghostty window as a named workspace — tabs, working directories, titles, and split panes
- Reopens any workspace as a fresh Ghostty window with native tabs
- Saves named directory entries, and reopens the current split as a fresh shell in that directory
- Launches from a keyboard-first TUI, or straight from the shell
- Aligns the new window to your current Ghostty window's position and size
- Renames, deletes, and searches without leaving the TUI
- Adapts its layout to the terminal width, down to a 36x15 terminal

## What it does not do

- Does not persist running processes
- Does not restore shell history, editor buffers, SSH sessions, or pane state
- Does not replace tmux for detach/attach, panes, or remote workflows

---

## The TUI

The TUI has two spaces, toggled with `f`:

- **Workspace space** — saved Ghostty window layouts. The list sits on the left, with a tab/split preview and quick settings beside it.
- **Directory space** — saved directory paths, shown as an adaptive multi-column grid.

Both spaces share the same keys wherever the action makes sense.

### Keys

**Move**

| Key | Action |
|-----|--------|
| `↑` `↓` / `j` `k` / `w` `s` | Move selection |
| `←` `→` | Move within a grid layout |
| `Home` / `End` `G` | Jump to first / last entry |
| `PgUp` / `PgDn` | Page up / down |

**Search**

| Key | Action |
|-----|--------|
| `/` | Start filtering the current space |
| `Tab` `Shift-Tab` / `Ctrl-n` `Ctrl-p` / `Ctrl-j` `Ctrl-k` | Move selection while filtering |
| `Enter` | Keep the filter |
| `Esc` | Revert the filter |

**Act**

| Key | Workspace space | Directory space |
|-----|-----------------|-----------------|
| `Enter` | Launch selected workspace | Replace the current split with a fresh shell in that directory |
| `a` | Save the current Ghostty window | Save the current shell directory |
| `n` | Rename selected | Rename selected |
| `d` | Delete selected | Delete selected |
| `e` | Open workspace file in `$EDITOR` | — |
| `g` | Edit the Ghostty shortcut | — |
| `t` | Open quick settings | — |

**Other**

| Key | Action |
|-----|--------|
| `r` | Reload from disk |
| `?` | Help |
| `q` | Quit |
| `Esc` | Clear an active filter, otherwise quit |

**Mouse:** click to select, double-click to run the primary action of the space (launch/replace), scroll wheel to move. In Workspace space, clicking the shortcut field opens the shortcut editor.

### Responsive layout

The workspace screen picks its layout from the available width, in this order:

1. **Wide** — list, layout preview, and quick settings side by side.
2. **Medium** — quick settings drops out; list plus preview.
3. **Narrow** — the preview moves below the list and workspace names wrap into columns.

Directory entries always render as a grid that fills the pane width; each column is sized to its own widest entry, so short names are not padded out to the longest one. Below 36x15 the TUI shows a "terminal too small" screen instead.

When you launch from the TUI, the new Ghostty window is repositioned to match your current window. This uses macOS Accessibility (System Events), so you may need to grant permission once.

---

## Directory space

Directory space stores named directory paths only. It does not rebuild Ghostty tabs or windows.

Press `f` to switch to it, `a` to save the current shell directory, and `Enter` (or double-click) to swap the current split for a fresh shell started in that directory.

This keeps Directory space zero-setup — upgrade gtab and use it immediately. Because it replaces the shell process in that split, in-flight shell state in the old split is discarded.

<details>
<summary>Shell-wrapper fallback (for use outside Ghostty)</summary>

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

`gtab --shell-cd` exists only for this wrapper flow. Other commands and workspace launches are unchanged.

</details>

---

## Commands

```text
gtab                     Open the TUI
gtab <name>              Launch a workspace directly
gtab save <name>         Save the current Ghostty window
gtab list                List saved workspaces
gtab rename <old> <new>  Rename a workspace
gtab remove <name>       Remove a workspace
gtab edit <name>         Open a workspace file in $EDITOR
gtab init                Enable the Ghostty-local Cmd+G shortcut
gtab shell-init <shell>  Print the shell integration for zsh, bash, or fish
gtab set                 Show current settings
gtab --version           Print the version
```

### Settings

```text
gtab set close_tab on|off               Auto-close the launching tab after launch
gtab set ghostty_shortcut <key>|off     Change or disable the Ghostty shortcut (e.g. cmd+shift+g)
gtab set shortcut_mode shell|text       Silent widget trigger, or type `gtab` into the shell
```

### Storage

Everything lives in `~/.config/gtab/`, or `$GTAB_DIR` when set:

```text
~/.config/gtab/
├── <name>.applescript      A saved workspace
├── dirs/<name>.path        A saved directory entry
├── config                  Settings
└── ghostty-shortcut.conf   Managed Ghostty keybind file
```

All plain text, all safe to read, edit, back up, or sync.

---

## Shortcut model

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

Nothing is echoed into the prompt, nothing enters shell history, and a half-typed command line is restored exactly as it was when the TUI exits. This is the same technique fzf and atuin use for their key bindings.

`gtab init` switches to this mode automatically once it finds a `gtab shell-init` line in your shell rc files. You can also set it explicitly with `gtab set shortcut_mode shell`. If shell mode is active but the integration is missing, `gtab init` and `gtab set` both warn that the shortcut will do nothing.

Upgrading from an older gtab changes nothing on its own: existing installs keep `shortcut_mode = text` until you install the integration and re-run `gtab init`.

### `shortcut_mode = text` (default, zero setup)

```conf
keybind = cmd+g=text:gtab\x0d
```

Ghostty types `gtab` into the active shell. No shell configuration needed, but the command stays visible on screen and lands in shell history.

> **Tradeoff (both modes):** the shortcut only reaches the shell, so it is not safe inside full-screen interactive programs like Claude Code, vim, or fzf — `text` mode types `gtab` into them, and `shell` mode sends a stray escape sequence. Quit those programs first, or run `gtab <name>` from a clean shell prompt.

### Nix / Home Manager

If your Ghostty config is read-only, `gtab init` still writes `~/.config/gtab/ghostty-shortcut.conf` and then tells you to add this line to your config source manually:

```conf
config-file = "/Users/you/.config/gtab/ghostty-shortcut.conf"
```

Rebuild/apply your config, then reload or restart Ghostty.

---

## How it works

`gtab save` reads the current Ghostty window through Ghostty's AppleScript API. For split-pane tabs, it also queries macOS Accessibility to capture pane positions, then reconstructs the split tree. The result is a plain `.applescript` file in `~/.config/gtab/`.

Launching a workspace runs that script through `osascript` to open a fresh Ghostty window and recreate the saved layout.

That is why `gtab` stays lightweight: it stores layout metadata, not live terminal session state.

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

## FAQ

<details>
<summary>Why does Cmd+G send text instead of running the binary directly?</summary>

Ghostty keybindings have no action for running external commands. The `text` action sends a string to the active shell, so that string is the only channel available.

`shortcut_mode = shell` uses that channel for an invisible trigger sequence rather than the literal word `gtab`, and lets a shell widget do the launching.

See [ghostty.org/docs/config/keybind](https://ghostty.org/docs/config/keybind).

</details>

<details>
<summary>The shortcut does nothing after switching to shortcut_mode = shell</summary>

The widget is missing. Run `gtab set` — in shell mode it checks your shell rc files and warns if no `gtab shell-init` line is there. Add the line back and start a new shell, or fall back with `gtab set shortcut_mode text`.

The check scans every common rc file without knowing which shell you are in, so if you use both zsh and bash but installed the integration for only one, the shortcut works only in that shell.

</details>

<details>
<summary>Why doesn't gtab edit my Nix / Home Manager config directly?</summary>

Nix and Home Manager generate the Ghostty config from a declaration source instead of a normal writable file. `gtab` can safely generate its own managed include file, but it cannot reliably edit every user's `home.nix`, flake module, or repo layout without risking a bad config change. In those setups, `gtab init` writes the managed include file and tells you exactly which `config-file = ...` line to add.

</details>

<details>
<summary>Does gtab support split panes?</summary>

Yes, since v1.4.1. `gtab save` captures split-pane layouts, and they are restored on launch.

</details>

<details>
<summary>My panes were evenly sized, but they come back uneven</summary>

Fixed in v1.8.1. Ghostty creates every split at 50/50, so three evenly sized panes used to reopen as 50/25/25. `gtab save` now compares the captured pane geometry against Ghostty's `equalize_splits` result and, when they match, records that action in the workspace script so even layouts reopen even.

Uneven layouts you arranged on purpose are still restored at Ghostty's default 50/50 per split. Ghostty's AppleScript API can only create splits, not set their exact ratios, so arbitrary proportions are not preserved yet.

</details>

---

## Uninstall

```bash
# 1. Disable the Ghostty shortcut, then reload the Ghostty config
gtab set ghostty_shortcut off

# 2. Remove the shell integration line from your shell rc, if you added one
#    eval "$(gtab shell-init zsh)"

# 3. Remove the binary
brew uninstall gtab        # or: cargo uninstall gtab

# 4. Optionally remove saved workspaces and settings
rm -rf ~/.config/gtab
```

---

## License

MIT
