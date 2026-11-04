use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// Builds a temp directory named `beams/` containing the one thing a
/// fresh beam needs that git itself won't carry over: a prebuilt
/// `beam-bootstrap` binary (no Rust toolchain on a fresh beam; see
/// bootstrap/README.md). Everything else - every git-tracked file in
/// `repo` - gets there via `tsh::push_repo` instead (see DESIGN.md's
/// "Repo delivery onto a beam").
///
/// No SSH private key is ever copied: the beam's agent is a relay to the
/// workstation's own agent (see SSH_AGENT_PROXY_DESIGN.md).
pub fn build(repo: &Path) -> anyhow::Result<PathBuf> {
    let tmp = std::env::temp_dir().join(format!("beams-client-stage-{}", std::process::id()));
    let dest_root = tmp.join("beams");
    fs::create_dir_all(&dest_root).with_context(|| format!("creating {}", dest_root.display()))?;

    let bin = find_bootstrap_bin(repo)?;
    let rel = bin.strip_prefix(repo).unwrap_or(&bin);
    copy_preserving_mode(&bin, &dest_root.join(rel))?;

    Ok(tmp)
}

/// Candidate paths under `bootstrap/target/`, most to least preferred. Beams
/// run Linux, so a cross-compiled target (built with `cargo zigbuild`, see
/// `bootstrap/build-cross.sh`) comes first - that's the only thing that can
/// work when the client itself runs on macOS. The plain `target/release` /
/// `target/debug` paths are listed last as a fallback for the case the
/// client *is* running natively on Linux (e.g. this sandbox) and just ran
/// `cargo build` directly, with no cross target involved at all.
const BIN_CANDIDATES: &[&str] = &[
    "bootstrap/target/x86_64-unknown-linux-musl/release/beam-bootstrap",
    "bootstrap/target/x86_64-unknown-linux-musl/debug/beam-bootstrap",
    "bootstrap/target/release/beam-bootstrap",
    "bootstrap/target/debug/beam-bootstrap",
];

fn find_bootstrap_bin(repo: &Path) -> anyhow::Result<PathBuf> {
    for candidate in BIN_CANDIDATES {
        let path = repo.join(candidate);
        if !path.exists() {
            continue;
        }
        if !is_elf(&path)? {
            // Most likely a native macOS build sitting at the fallback
            // `target/release` path - useless on a Linux beam. Keep
            // looking rather than silently shipping a binary that will
            // just fail to exec once it gets there.
            continue;
        }
        return Ok(path);
    }
    bail!(
        "no usable (Linux ELF) beam-bootstrap binary found under {}/bootstrap/target - \
         cross-compile one first, see bootstrap/build-cross.sh",
        repo.display()
    );
}

fn is_elf(path: &Path) -> anyhow::Result<bool> {
    let mut magic = [0u8; 4];
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    use std::io::Read;
    if f.read_exact(&mut magic).is_err() {
        return Ok(false);
    }
    Ok(magic == [0x7f, b'E', b'L', b'F'])
}

fn copy_preserving_mode(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(src, dest).with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
    let mode = fs::metadata(src)
        .with_context(|| format!("reading permissions of {}", src.display()))?
        .permissions()
        .mode();
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn prefers_cross_compiled_elf_over_native_fallback() {
        let repo = std::env::temp_dir().join(format!("stage-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);

        // A native macOS build sitting at the fallback path (Mach-O magic,
        // not ELF) should be skipped in favor of the cross-compiled one.
        write(
            &repo.join("bootstrap/target/release/beam-bootstrap"),
            &[0xcf, 0xfa, 0xed, 0xfe],
        );
        write(
            &repo.join("bootstrap/target/x86_64-unknown-linux-musl/release/beam-bootstrap"),
            &[0x7f, b'E', b'L', b'F', 0, 0],
        );

        let found = find_bootstrap_bin(&repo).unwrap();
        assert!(found.ends_with("x86_64-unknown-linux-musl/release/beam-bootstrap"));

        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn falls_back_to_native_when_no_cross_build_exists() {
        let repo = std::env::temp_dir().join(format!("stage-test-fallback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);

        write(
            &repo.join("bootstrap/target/release/beam-bootstrap"),
            &[0x7f, b'E', b'L', b'F', 0, 0],
        );

        let found = find_bootstrap_bin(&repo).unwrap();
        assert!(found.ends_with("bootstrap/target/release/beam-bootstrap"));

        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn errors_when_only_a_non_elf_binary_is_present() {
        let repo = std::env::temp_dir().join(format!("stage-test-noelf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);

        write(
            &repo.join("bootstrap/target/release/beam-bootstrap"),
            &[0xcf, 0xfa, 0xed, 0xfe],
        );

        assert!(find_bootstrap_bin(&repo).is_err());

        fs::remove_dir_all(&repo).unwrap();
    }
}
