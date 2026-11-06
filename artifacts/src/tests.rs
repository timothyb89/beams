use super::*;
use beams_protocol::launch::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

struct Fixture(TemporaryDirectory);
impl Fixture {
    fn new() -> Self {
        Self(TemporaryDirectory::new(&std::env::temp_dir()).unwrap())
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0 .0.join(name)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}
fn declared(artifacts: Vec<Artifact>) -> Declaration {
    declaration(LaunchPlan {
        schema_version: 1,
        shell: Shell::default(),
        env: BTreeMap::new(),
        nodes: vec![],
        artifacts,
    })
    .unwrap()
}
fn artifact(id: &str, directory: bool, phase: Phase) -> Artifact {
    Artifact {
        id: id.into(),
        directory,
        phase,
        exclude: vec![],
        used_by: vec![],
    }
}

#[test]
fn phase_snapshots_defer_post_and_spools_replay_after_source_edits() {
    let f = Fixture::new();
    let source = f.write("source/pre.sh", b"echo original");
    let launch = declared(vec![
        artifact("pre", false, Phase::Pre),
        artifact("post", true, Phase::Post),
    ]);
    let sources = BTreeMap::from([
        ("pre".into(), Selection::Local(source.clone())),
        ("post".into(), Selection::Local(f.path("missing-post-root"))),
    ]);
    let archive = f.path("pre.tar.gz");
    let desc = pack(&launch, Phase::Pre, &sources, &archive, Compression::Gzip).unwrap();
    fs::write(source, b"echo changed").unwrap();
    let stage = f.path("stage");
    let manifest = receive(&archive, &desc, &stage).unwrap();
    assert_eq!(manifest.artifacts.len(), 1);
    assert_eq!(
        fs::read(stage.join(&manifest.artifacts[0].root)).unwrap(),
        b"echo original"
    );
    receive(&archive, &desc, &stage).unwrap();
    assert!(pack(
        &launch,
        Phase::Post,
        &sources,
        &f.path("post.tar.gz"),
        Compression::Gzip
    )
    .is_err());
}

#[test]
fn both_codecs_are_deterministic_and_preserve_empty_dirs_long_names_modes_and_internal_links() {
    for compression in [Compression::None, Compression::Gzip] {
        let f = Fixture::new();
        f.write("source/hooks/main.sh", b"echo hello");
        set_mode(&f.path("source/hooks/main.sh"), 0o751).unwrap();
        fs::create_dir(f.path("source/empty")).unwrap();
        let long = "x".repeat(140);
        f.write(&format!("source/{long}/long.txt"), b"long path");
        f.write("source/.git/secret", b"excluded");
        #[cfg(unix)]
        std::os::unix::fs::symlink("hooks/main.sh", f.path("source/entry")).unwrap();
        let mut selection = artifact("tree", true, Phase::Pre);
        selection.exclude.push(".git".into());
        let launch = declared(vec![selection]);
        let sources = BTreeMap::from([("tree".into(), Selection::Local(f.path("source")))]);
        let a = f.path("one.tar");
        let b = f.path("two.tar");
        let first = pack(&launch, Phase::Pre, &sources, &a, compression).unwrap();
        let second = pack(&launch, Phase::Pre, &sources, &b, compression).unwrap();
        assert_eq!(first.sha256, second.sha256);
        let stage = f.path("stage");
        let manifest = receive(&a, &first, &stage).unwrap();
        assert!(stage.join("payload/tree/empty").is_dir());
        assert!(!stage.join("payload/tree/.git").exists());
        assert_eq!(
            fs::read(stage.join(format!("payload/tree/{long}/long.txt"))).unwrap(),
            b"long path"
        );
        let file = manifest.artifacts[0]
            .entries
            .iter()
            .find(|e| e.path.ends_with("main.sh"))
            .unwrap();
        assert_eq!(file.mode, 0o751);
        assert_eq!(mode(&fs::metadata(stage.join(&file.path)).unwrap()), 0o555);
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(stage.join("payload/tree/entry")).unwrap(),
            Path::new("hooks/main.sh")
        );
    }
}

#[test]
fn symlink_escape_cycles_excluded_targets_and_special_files_fail_without_publishing() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        for target in ["/etc/passwd", "../../outside", "missing", "entry"] {
            let f = Fixture::new();
            f.write("source/file", b"data");
            symlink(target, f.path("source/entry")).unwrap();
            let launch = declared(vec![artifact("tree", true, Phase::Pre)]);
            let sources = BTreeMap::from([("tree".into(), Selection::Local(f.path("source")))]);
            assert!(pack(
                &launch,
                Phase::Pre,
                &sources,
                &f.path("bundle"),
                Compression::None
            )
            .is_err());
            assert!(!f.path("bundle").exists());
        }
        let f = Fixture::new();
        f.write("source/.git/file", b"excluded");
        symlink(".git/file", f.path("source/link")).unwrap();
        let mut tree = artifact("tree", true, Phase::Pre);
        tree.exclude.push(".git".into());
        assert!(pack(
            &declared(vec![tree]),
            Phase::Pre,
            &BTreeMap::from([("tree".into(), Selection::Local(f.path("source")))]),
            &f.path("bundle"),
            Compression::None
        )
        .is_err());
        let f = Fixture::new();
        fs::create_dir(f.path("source")).unwrap();
        let name = std::ffi::CString::new(f.path("source/fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(pack(
            &declared(vec![artifact("tree", true, Phase::Pre)]),
            Phase::Pre,
            &BTreeMap::from([("tree".into(), Selection::Local(f.path("source")))]),
            &f.path("bundle"),
            Compression::None
        )
        .is_err());
    }
}

#[test]
fn receipt_rejects_hash_binding_truncation_and_modified_staging() {
    let f = Fixture::new();
    let source = f.write("input", b"hello");
    let launch = declared(vec![artifact("file", false, Phase::Pre)]);
    let archive = f.path("bundle");
    let desc = pack(
        &launch,
        Phase::Pre,
        &BTreeMap::from([("file".into(), Selection::Local(source))]),
        &archive,
        Compression::Gzip,
    )
    .unwrap();
    let mut wrong = desc.clone();
    wrong.sha256 = "a".repeat(64);
    assert!(receive(&archive, &wrong, &f.path("bad")).is_err());
    assert!(!f.path("bad").exists());
    wrong = desc.clone();
    wrong.phase = Phase::Post;
    assert!(receive(&archive, &wrong, &f.path("bad")).is_err());
    let original = fs::read(&archive).unwrap();
    let short = f.write("short", &original[..original.len() / 2]);
    assert!(receive(&short, &desc, &f.path("bad")).is_err());
    let stage = f.path("stage");
    let manifest = receive(&archive, &desc, &stage).unwrap();
    let payload = stage.join(&manifest.artifacts[0].root);
    set_mode(&payload, 0o600).unwrap();
    fs::write(payload, b"changed").unwrap();
    assert!(receive(&archive, &desc, &stage).is_err());
}

#[test]
fn malformed_manifest_unknown_entries_and_hardlinks_never_publish() {
    let f = Fixture::new();
    let launch = declared(vec![]);
    let manifest = BundleManifest {
        schema_version: 1,
        launch_id: launch.launch_id.clone(),
        plan_digest: launch.plan_digest.clone(),
        phase: Phase::Pre,
        artifacts: vec![],
    };
    for (name, kind) in [
        ("payload/unknown", tar::EntryType::Regular),
        ("payload/link", tar::EntryType::Link),
    ] {
        let path = f.path("malformed");
        let mut tar = tar::Builder::new(File::create(&path).unwrap());
        let json = serde_json::to_vec(&manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(json.len() as u64);
        header.set_mode(0o600);
        tar.append_data(&mut header, "bundle.json", json.as_slice())
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o600);
        header.set_entry_type(kind);
        if kind.is_hard_link() {
            header.set_link_name("bundle.json").unwrap();
        }
        tar.append_data(&mut header, name, std::io::empty())
            .unwrap();
        tar.finish().unwrap();
        drop(tar);
        let (sha256, bytes) = hash_file(&path).unwrap();
        let desc = BundleDescriptor {
            schema_version: 1,
            launch_id: launch.launch_id.clone(),
            plan_digest: launch.plan_digest.clone(),
            phase: Phase::Pre,
            compression: Compression::None,
            sha256,
            bytes,
        };
        assert!(receive(&path, &desc, &f.path("stage")).is_err());
        assert!(!f.path("stage").exists());
    }
    for path in [
        "../escape",
        "/absolute",
        "payload/../escape",
        "payload//file",
        "payload/./file",
    ] {
        assert!(safe_path(path).is_err());
    }
}
