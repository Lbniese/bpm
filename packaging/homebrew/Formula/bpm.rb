class Bpm < Formula
  desc "Bloom Package Manager: an npm-compatible, performance-focused package installer"
  homepage "https://lbniese.github.io/bpm/"
  url "https://github.com/Lbniese/bpm/releases/download/v0.5.0/bpm-x86_64-apple-darwin.tar.gz"
  sha256 "667f7b793b4afbaf1cda92b3bbe7c084bdad73663247491b01bed3e301ca718f"
  license "MIT"

  # Release tarballs are a single prebuilt `bpm` binary per target; the
  # checksums come from the signed SHA256SUMS on each release.
  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.0/bpm-x86_64-apple-darwin.tar.gz"
      sha256 "667f7b793b4afbaf1cda92b3bbe7c084bdad73663247491b01bed3e301ca718f"
    else
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.0/bpm-aarch64-apple-darwin.tar.gz"
      sha256 "f0ecb226835db71983178aef2d7ea360a5e2f7df6ebba6be2c9f3f79a7cfc8fc"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.0/bpm-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "151528f86d262820c8ea64b225106df3b9dc3c86605f1e26e7144ade1574c3b0"
    else
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.0/bpm-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "96b8c023d82b5f2eb9c5e8be36ddf38c299d2c9898dbd3400ceda25e47e97cb7"
    end
  end

  def install
    bin.install "bpm"
  end

  def caveats
    <<~EOS
      The global store lives in ~/.bpm by default; set BPM_STORE to relocate it.
      Run `bpm doctor` inside a project to inspect its configuration.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/bpm --version")
    assert_match "bpm.lock", shell_output("#{bin}/bpm import --help")
  end
end
