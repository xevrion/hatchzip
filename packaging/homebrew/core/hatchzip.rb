# For homebrew-core, which takes only formulae built from source, and only
# once the repository and the homepage are 30 days old (2026-11-23). Only
# the command line tool: the desktop app stays a cask in xevrion/tap. No
# `hz` symlink: homebrew-core's `hz` formula already owns bin/hz. Checked
# with `brew audit --strict --online`, `brew style`, a source build and
# `brew test` (Homebrew 7.0.11, 2026-10-30).
class Hatchzip < Formula
  desc "Extract huge archives in place, needing free space only for the contents"
  homepage "https://hatchzip.dev"
  url "https://github.com/xevrion/hatchzip/archive/refs/tags/v0.1.8.tar.gz"
  sha256 "10b9113051754cf8aa17d5eeed9474909aed3841c6e8657f4e98b0fdcfc9454c"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/xevrion/hatchzip.git", branch: "main"

  depends_on "rust" => :build

  deny_network_access!

  def fetch
    system "cargo", "fetch", *std_cargo_fetch_args
  end

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/cli")

    generate_completions_from_executable(bin/"hatchzip", "completions")
    (man1/"hatchzip.1").write Utils.safe_popen_read(bin/"hatchzip", "man")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hatchzip --version")

    # A real extraction: the file comes out and the archive is gone.
    (testpath/"hello.txt").write "hatched\n"
    system "tar", "-cf", "test.tar", "hello.txt"
    rm "hello.txt"
    system bin/"hatchzip", "extract", "--yes", "--keep-free", "0", "test.tar"
    assert_equal "hatched\n", (testpath/"test/hello.txt").read
    refute_path_exists testpath/"test.tar"
  end
end
