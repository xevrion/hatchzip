# The command line tool from the prebuilt release tarballs, for the tap
# xevrion/homebrew-tap. The desktop app is the cask in ../Casks.
class Hatchzip < Formula
  desc "Extract huge archives in place, needing free space only for the contents"
  homepage "https://hatchzip.dev"
  license any_of: ["MIT", "Apache-2.0"]

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    on_arm do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.9/hatchzip-aarch64-macos.tar.gz"
      sha256 "6d2922ebfd2b06c4281ae6bc324ab979d728f19f3a5bfd29e37945fdddee58f3"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.9/hatchzip-x86_64-macos.tar.gz"
      sha256 "c7b5bcec524f13a3942779dce0d059b120f72de1826122543bc6ddde02dc49c2"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.9/hatchzip-aarch64-linux.tar.gz"
      sha256 "5021d38faafc33273637827d0e145947ee72ec053f4b67e1c2882a0fe7e1718f"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.9/hatchzip-x86_64-linux.tar.gz"
      sha256 "6041a3c438d81b15ada569f3d0851d3173f1b3eab03442ed817517b1e7b731da"
    end
  end

  def install
    bin.install "hatchzip"
    bin.install_symlink "hatchzip" => "hz"
    man1.install "man/hatchzip.1"
    man1.install_symlink "hatchzip.1" => "hz.1"
    bash_completion.install "completions/hatchzip.bash" => "hatchzip"
    zsh_completion.install "completions/_hatchzip"
    fish_completion.install "completions/hatchzip.fish"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hatchzip --version")

    # A real extraction: the file comes out and the archive is gone.
    (testpath/"hello.txt").write "hatched\n"
    system "tar", "-cf", "test.tar", "hello.txt"
    rm "hello.txt"
    system bin/"hz", "extract", "--yes", "--keep-free", "0", "test.tar"
    assert_equal "hatched\n", (testpath/"test/hello.txt").read
    refute_path_exists testpath/"test.tar"
  end
end
