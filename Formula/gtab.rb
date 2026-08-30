class Gtab < Formula
  desc "Ghostty tab workspace manager with an interactive TUI"
  homepage "https://github.com/Franvy/gtab"
  url "https://github.com/Franvy/gtab.git",
      tag: "v1.8.1"
  version "1.8.1"
  license "MIT"
  head "https://github.com/Franvy/gtab.git", branch: "main"

  depends_on :macos
  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      Run this once to enable the default Ghostty-local Cmd+G:
        gtab init

      Workspaces are stored in ~/.config/gtab/ by default.
      Override with: export GTAB_DIR="/your/path"

      Requires Ghostty terminal: https://ghostty.org
    EOS
  end

  test do
    ENV["GTAB_DIR"] = testpath/"gtab"
    (testpath/"gtab").mkpath
    (testpath/"gtab/demo.applescript").write <<~APPLESCRIPT
      tell application "Ghostty"
      end tell
    APPLESCRIPT

    assert_match version.to_s, shell_output("#{bin}/gtab --version")
    assert_match "demo", shell_output("#{bin}/gtab list")
    assert_match "close_tab = off", shell_output("#{bin}/gtab set")
    assert_match "ghostty_shortcut = cmd+g", shell_output("#{bin}/gtab set")
    assert_match "shortcut_mode = text", shell_output("#{bin}/gtab set")

    assert_match "_gtab_launch_widget", shell_output("#{bin}/gtab shell-init zsh")
    assert_match "unsupported shell", shell_output("#{bin}/gtab shell-init tcsh 2>&1", 1)

    system bin/"gtab", "set", "close_tab", "on"
    assert_match "close_tab = on", shell_output("#{bin}/gtab set")

    assert_match "launch_mode has been removed", shell_output("#{bin}/gtab set launch_mode window", 1)
  end
end
