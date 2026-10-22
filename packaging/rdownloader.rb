class Rdownloader < Formula
  desc "Clean-room Rust download manager with Tor"
  homepage "https://github.com/Benjamin-Wegener/rDownloader"
  url "https://github.com/Benjamin-Wegener/rDownloader/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_SHA256"
  license "MIT"
  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    system "#{bin}/rdownloader", "--version"
  end
end
