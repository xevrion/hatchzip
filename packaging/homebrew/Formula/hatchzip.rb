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
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.11/hatchzip-aarch64-macos.tar.gz"
      sha256 "f855d25be52850e3a7ab72a68cf4252f7702b4b79ea5d1b67f4a7fea7d81e927"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.11/hatchzip-x86_64-macos.tar.gz"
      sha256 "a9bc222bb166c62dea910914d4552e71214e0f63b273f6c13b61867bdfae8433"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.11/hatchzip-aarch64-linux.tar.gz"
      sha256 "aaea8e37f475dfa51569a38c759ef934a29b7511939618d370963b8304748a5e"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.11/hatchzip-x86_64-linux.tar.gz"
      sha256 "4bbba16848cebe8197477bf78527eaf8dfdc8cc20dafa09b217c4e161a4cb140"
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
