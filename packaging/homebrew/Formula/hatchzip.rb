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
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.10/hatchzip-aarch64-macos.tar.gz"
      sha256 "5620aef1de43f9caf8a83fddc39020514ed82c57ec91b7a8df6e97c93bbbfc10"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.10/hatchzip-x86_64-macos.tar.gz"
      sha256 "3559acfbbdd02255de08f677ac9cbab96415111d2ccdda18e4b759667937957f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.10/hatchzip-aarch64-linux.tar.gz"
      sha256 "e5edc2277e51df7130976fa250a3f6451e053eb2de678a1f600bee87510edeca"
    end
    on_intel do
      url "https://github.com/xevrion/hatchzip/releases/download/v0.1.10/hatchzip-x86_64-linux.tar.gz"
      sha256 "508b9534e2451040745de5f1cb140ee2d050e8ed114e280158d8110b392ab929"
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
