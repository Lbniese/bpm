class Bpm < Formula
  desc "Bloom Package Manager: an npm-compatible, performance-focused package installer"
  homepage "https://lbniese.github.io/bpm/"
  url "https://github.com/Lbniese/bpm/releases/download/v0.5.1/bpm-x86_64-apple-darwin.tar.gz"
  sha256 "ed892f9f43adfd9bf6e2473774135a20affdc5680fb6eaa7528de4c425634d22"
  license "MIT"

  # Release tarballs are a single prebuilt `bpm` binary per target; the
  # checksums come from the signed SHA256SUMS on each release.
  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.1/bpm-x86_64-apple-darwin.tar.gz"
      sha256 "ed892f9f43adfd9bf6e2473774135a20affdc5680fb6eaa7528de4c425634d22"
    else
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.1/bpm-aarch64-apple-darwin.tar.gz"
      sha256 "1fc24fa417f91bedb082a8280dbf1610de59e20c4d48d2e3dee35f1dbfcae9ad"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.1/bpm-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "8b3d2998e1d07215f4d8354d3232da4e32d6f110ba6228b241b5af5100792e1a"
    else
      url "https://github.com/Lbniese/bpm/releases/download/v0.5.1/bpm-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a0e9795559013658083613dc7c7e2c6819a1d1d7a7372105488082a9a3405c3f"
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
