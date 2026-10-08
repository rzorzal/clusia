class Clusia < Formula
  desc "PR reviews for humans: macOS app, menu bar tray and CLI"
  homepage "https://github.com/rzorzal/clusia"
  license "Apache-2.0"
  head "https://github.com/rzorzal/clusia.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    # `clusia install` assembles Clusia.app from these four programs.
    %w[clusia clusiad clusia-tray clusia-app].each do |crate|
      system "cargo", "install", *std_cargo_args(root: libexec, path: "crates/#{crate}")
    end
    bin.install_symlink libexec/"bin/clusia"
  end

  def caveats
    <<~EOS
      Homebrew builds Clúsia but cannot write to /Applications or ~/Library, so finish
      the install once with:

        clusia install --from #{opt_libexec}/bin

      That puts Clusia.app in /Applications, registers the daemon to start at login and
      links `clusia` for your terminal. Run it again after every `brew upgrade`.

      A few seconds after you open Clusia.app, macOS asks to allow notifications.
      Choose Allow; you can change it later in System Settings › Notifications.

      Before `brew uninstall clusia`, run `clusia uninstall`. It removes what `clusia install`
      created and keeps your reviews and settings in ~/Library/Application Support/Clusia.
    EOS
  end

  test do
    assert_match "install", shell_output("#{bin}/clusia --help")
    output = shell_output("#{bin}/clusia install --dry-run --from #{libexec}/bin")
    assert_match "Clusia.app", output
  end
end
