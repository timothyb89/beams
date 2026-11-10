use std::path::{Path, PathBuf};

use beams_protocol::ManifestParam;

use super::{
    explain,
    plan::{self, Action, Compilation, Node},
    profile::{Phase, Source},
};

fn param_str(value: &ManifestParam) -> &str {
    match value {
        ManifestParam::Str(v) => v,
        _ => panic!("expected a string param"),
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("beams-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }
    fn compile(&self, text: &str) -> anyhow::Result<Compilation> {
        let path = self.write("environment.toml", &format!("schema_version = 1\n{text}"));
        plan::compile(path.to_str().unwrap())
    }
    fn error(&self, text: &str) -> String {
        format!("{:#}", self.compile(text).unwrap_err())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node<'a>(compiled: &'a Compilation, id: &str) -> &'a Node {
    compiled.plan.nodes.iter().find(|n| n.id == id).unwrap()
}
fn packages(compiled: &Compilation, phase: Phase) -> &[String] {
    match &node(compiled, &format!("packages:{}", phase.name())).action {
        Action::Packages { packages } => packages,
        _ => panic!("expected packages"),
    }
}
fn depends(compiled: &Compilation, from: &str, to: &str) -> bool {
    node(compiled, from).after.iter().any(|dep| dep == to)
}
fn source_path(source: &Source) -> &Path {
    match source {
        Source::Local(p) => p,
        _ => panic!("expected local source"),
    }
}

#[test]
fn bundled_profiles_have_no_checkout_dependency_and_minimal_pre_work() {
    let core = plan::compile("builtin:core").unwrap();
    assert!(core.plan.artifacts.is_empty());
    assert!(packages(&core, Phase::Pre).is_empty());
    let dev = plan::compile("builtin:development").unwrap();
    assert_eq!(packages(&dev, Phase::Pre), ["fish"]);
    assert_eq!(packages(&dev, Phase::Post), ["micro", "less", "gcc"]);
    assert!(depends(&dev, "tasks:rust", "packages:post"));
    assert!(!depends(&dev, "tasks:go", "packages:post"));
    assert!(!depends(&dev, "interface-ready", "tasks:rust"));
    assert!(depends(&dev, "interface-ready", "files:fish"));
    assert_eq!(node(&dev, "tasks:starship").phase, Phase::Pre, "the first prompt has it");
    assert!(depends(&dev, "interface-ready", "tasks:starship"));
    assert!(dev.sources.values().all(|s| match s {
        Source::Embedded(name) => !super::profile::embedded_asset(name).unwrap().is_empty(),
        _ => false,
    }));
}

#[test]
fn builtin_core_configures_git_identity_from_the_workstation_by_default() {
    // builtin:core declares this task with no explicit params - the
    // compiler fills whatever the workstation's own `git config` has
    // (nothing, on a workstation with no git identity, is just as
    // valid a result as something).
    let core = plan::compile("builtin:core").unwrap();
    let Action::Builtin { template, params } = &node(&core, "tasks:git_identity").action else {
        panic!("expected a builtin task");
    };
    assert_eq!(template, "configure-git-identity");
    for key in params.keys() {
        assert!(["name", "email"].contains(&key.as_str()));
    }
}

#[test]
fn a_profiles_explicit_git_identity_params_are_not_overridden_by_the_workstations() {
    let fixture = Fixture::new();
    let compiled = fixture
        .compile(
            "[tasks.git_identity]\n\
             template = \"configure-git-identity\"\n\
             phase = \"pre\"\n\
             [tasks.git_identity.params]\n\
             name = \"Override Name\"\n\
             email = \"override@example.invalid\"\n",
        )
        .unwrap();
    let Action::Builtin { params, .. } = &node(&compiled, "tasks:git_identity").action else {
        panic!("expected a builtin task");
    };
    assert_eq!(param_str(&params["name"]), "Override Name");
    assert_eq!(param_str(&params["email"]), "override@example.invalid");
}

#[test]
fn documented_team_fixture_proves_packages_scripts_files_and_shared_root_ordering() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../environments/examples/team/environment.toml");
    let compiled = plan::compile(fixture.to_str().unwrap()).unwrap();
    assert!(!compiled.plan.nodes.iter().any(|n| n.id == "tasks:go"));
    assert_eq!(
        packages(&compiled, Phase::Post),
        ["micro", "less", "jq", "curl", "gcc"]
    );
    assert!(depends(&compiled, "tasks:prepare", "files:fish"));
    assert!(!depends(&compiled, "tasks:prepare", "packages:pre"));
    assert!(depends(&compiled, "interface-ready", "tasks:prepare"));
    assert!(depends(&compiled, "tasks:configure", "packages:post"));
    assert!(depends(&compiled, "tasks:configure", "files:data"));
    assert!(depends(&compiled, "tasks:configure", "artifact-ready:pre"));
    assert!(!depends(
        &compiled,
        "tasks:configure",
        "artifact-ready:post"
    ));
    assert!(depends(&compiled, "files:data", "artifact-ready:post"));
    let root = compiled
        .plan
        .artifacts
        .iter()
        .find(|a| a.used_by.contains(&"tasks:prepare".into()))
        .unwrap();
    assert_eq!(root.phase, Phase::Pre);
    assert_eq!(root.used_by, ["tasks:configure", "tasks:prepare"]);
    assert!(compiled.notes.iter().any(|n| n.contains("promoted to pre")));
    let output = explain(&compiled);
    for expected in [
        "tasks:prepare [pre]",
        "tasks:configure [post]",
        "packages:post [post]",
        "packages: micro, less, jq, curl, gcc",
        "artifact-ready:pre",
    ] {
        assert!(output.contains(expected), "missing {expected} in {output}");
    }
    let guide = include_str!("../../../docs/development/launches.md");
    for line in [
        "tasks:prepare [pre] Prepare shell files (script: artifact-4/prepare.sh)",
        "packages:pre [pre] pre packages (packages: fish)",
        "packages:post [post] post packages (packages: micro, less, jq, curl, gcc)",
        "tasks:configure [post] Configure project data (script: artifact-4/configure.sh)",
    ] {
        assert!(
            output.contains(line) && guide.contains(line),
            "guide and explain must agree on {line}"
        );
    }
}

#[test]
fn inherited_sources_keep_origins_and_named_entries_replace_atomically() {
    let f = Fixture::new();
    let hook = f.write("base/hooks/setup.sh", "#!/bin/sh\ntrue");
    f.write(
        "base/base.toml",
        r#"schema_version = 1
[tasks.inherited]
script = "./hooks/setup.sh"
packages = ["jq"]
[tasks.replaced]
script = "./hooks/setup.sh"
packages = ["curl"]
"#,
    );
    let compiled = f
        .compile(
            r#"extends = ["base/base.toml"]
[tasks.replaced]
template = "install-toolchain-go"
"#,
        )
        .unwrap();
    let Action::Script { artifact, .. } = &node(&compiled, "tasks:inherited").action else {
        panic!()
    };
    assert_eq!(source_path(&compiled.sources[artifact]), hook);
    assert_eq!(packages(&compiled, Phase::Post), ["jq"]);
    assert!(compiled.origins["tasks:inherited"]
        .profile
        .ends_with("base/base.toml"));
}

#[test]
fn multiple_bases_apply_in_order_and_environment_path_arrays_merge_as_documented() {
    let f = Fixture::new();
    f.write(
        "a.toml",
        "schema_version = 1\n[env]\nA = 'a'\nB = 'a'\n[shell]\npath = ['~/a']",
    );
    f.write(
        "b.toml",
        "schema_version = 1\n[env]\nB = 'b'\n[shell]\npath = ['~/b']",
    );
    let compiled = f
        .compile("extends = ['a.toml', 'b.toml']\n[env]\nC = 'child'")
        .unwrap();
    assert_eq!(compiled.plan.env["A"], "a");
    assert_eq!(compiled.plan.env["B"], "b");
    assert_eq!(compiled.plan.env["C"], "child");
    assert_eq!(compiled.plan.shell.path.unwrap(), ["${beam.home}/b"]);
}

#[test]
fn inheritance_cycle_unknown_fields_and_schema_versions_fail_clearly() {
    let f = Fixture::new();
    f.write(
        "base.toml",
        "schema_version = 1\nextends = ['environment.toml']",
    );
    assert!(f
        .error("extends = ['base.toml']")
        .contains("inheritance cycle"));
    assert!(f
        .error("[tasks.bad]\nscript = 'missing.sh'\npackges = ['jq']")
        .contains("unknown field `packges`"));
    let path = f.write("v2.toml", "schema_version = 2");
    assert!(plan::compile(path.to_str().unwrap())
        .unwrap_err()
        .to_string()
        .contains("schema_version"));
}

#[test]
fn packages_union_promote_pin_and_remove() {
    let f = Fixture::new();
    f.write(
        "base.toml",
        "schema_version = 1\n[packages.post]\nadd = ['jq', 'curl', 'less']",
    );
    let compiled = f
        .compile(
            r#"extends = ["base.toml"]
[packages.pre]
add = ["jq=1.7-1"]
[packages.post]
add = ["curl", "jq"]
remove = ["less"]
[tasks.consumer]
template = "install-toolchain-go"
packages = ["jq"]
"#,
        )
        .unwrap();
    assert_eq!(packages(&compiled, Phase::Pre), ["jq=1.7-1"]);
    assert_eq!(packages(&compiled, Phase::Post), ["curl"]);
    assert!(depends(&compiled, "tasks:consumer", "packages:pre"));
    assert!(!depends(&compiled, "tasks:consumer", "packages:post"));
    assert!(f
        .error("[packages.pre]\nadd=['jq=1']\n[packages.post]\nadd=['jq=2']")
        .contains("conflicting package versions"));
    assert!(f
        .error("[packages.post]\nadd=['--allow-unauthenticated']")
        .contains("invalid package"));
    assert!(f
        .error("[packages.post]\nremove=['gcc']\n[tasks.rust]\ntemplate='install-toolchain-rust'")
        .contains("required package gcc was removed"));
    let moved = f.compile("[packages.pre]\nadd=['gcc']\n[packages.post]\nremove=['gcc']\n[tasks.rust]\ntemplate='install-toolchain-rust'").unwrap();
    assert!(depends(&moved, "tasks:rust", "packages:pre"));
}

#[test]
fn scripts_need_explicit_helpers_and_package_edges() {
    let f = Fixture::new();
    let script = f.write("hooks/main.sh", "true");
    f.write("hooks/helper.sh", "false");
    let single = f.compile("[tasks.single]\nscript='hooks/main.sh'").unwrap();
    assert_eq!(single.sources.len(), 1);
    assert_eq!(source_path(single.sources.values().next().unwrap()), script);
    assert!(!single.plan.artifacts[0].directory);
    assert!(!depends(&single, "tasks:single", "packages:post"));
    let root = f
        .compile("[tasks.root]\nscript='hooks/main.sh'\nscript_root='hooks'\npackages=['jq']")
        .unwrap();
    assert!(root.plan.artifacts[0].directory);
    assert_eq!(
        source_path(root.sources.values().next().unwrap()),
        f.0.join("hooks")
    );
    assert!(depends(&root, "tasks:root", "packages:post"));
    let Action::Script {
        entrypoint,
        working_dir,
        interpreter,
        ..
    } = &node(&root, "tasks:root").action
    else {
        panic!()
    };
    assert_eq!(entrypoint, "main.sh");
    assert!(working_dir.is_none());
    assert_eq!(interpreter, &["bash"]);
}

#[test]
fn missing_sources_outside_roots_and_excluded_entrypoints_fail() {
    let f = Fixture::new();
    f.write("main.sh", "true");
    f.write("helpers/helper.sh", "true");
    assert!(f
        .error("[tasks.bad]\nscript='missing.sh'")
        .contains("missing source"));
    assert!(f
        .error("[tasks.bad]\nscript='main.sh'\nscript_root='helpers'")
        .contains("inside script_root"));
    assert!(f
        .error("[tasks.bad]\nscript='main.sh'\nexclude=['main.sh']")
        .contains("entrypoint is excluded"));
    assert!(f
        .error("[tasks.bad]\nscript='main.sh'\nexclude=['../other']")
        .contains("relative path prefixes"));
    assert!(f
        .error("[tasks.bad]\nscript='helpers'")
        .contains("script must be a file"));
    assert!(f
        .error("[tasks.bad]\nscript='main.sh'\ninterpreter=[]")
        .contains("nonempty executable"));
}

#[test]
fn builtin_params_auto_prerequisites_and_missing_repo_provider_are_validated() {
    let f = Fixture::new();
    let code = f
        .compile("[tasks.code]\ntemplate='install-code-server'")
        .unwrap();
    assert!(depends(
        &code,
        "tasks:code",
        "tasks:auto-install-toolchain-go"
    ));
    assert!(depends(
        &code,
        "tasks:code",
        "tasks:auto-install-toolchain-rust"
    ));
    let clone = f.compile("[tasks.repo]\ntemplate='clone-git-repo'\nparams={repo='git@example.com:team/repo', dest='~/repo'}").unwrap();
    assert!(depends(&clone, "tasks:repo", "tasks:auto-wait-ssh-agent"));
    assert!(f
        .error("[tasks.repo]\ntemplate='clone-git-repo'\nparams={repo='https://example.com'}")
        .contains("missing parameter dest"));
    assert!(f.error("[tasks.repo]\ntemplate='clone-git-repo'\nparams={repo='https://example.com',dest='~/repo',typo=1}").contains("unknown parameter"));
    assert!(f
        .error(
            "[tasks.pr]\ntemplate='checkout-pr-worktree'\nparams={base='~/repo',dest='~/pr',pr=-1}"
        )
        .contains("pr must be"));
    assert!(f
        .error(
            "[tasks.pr]\ntemplate='checkout-pr-worktree'\nparams={base='~/repo',dest='~/pr',pr=1}"
        )
        .contains("missing provider"));
    assert!(f
        .error("[tasks.bad]\ntemplate='not-a-template'")
        .contains("unknown built-in template"));
}

#[test]
fn capability_matching_normalizes_remote_paths() {
    let f = Fixture::new();
    let compiled = f
        .compile(
            r#"
[tasks.repo]
template = "clone-git-repo"
params = { repo = "https://example.com/repo", dest = "~/work/../repo" }
[tasks.pr]
template = "checkout-pr-worktree"
params = { base = "${beam.home}/repo", dest = "~/pr", pr = 123 }
"#,
        )
        .unwrap();
    assert!(depends(&compiled, "tasks:pr", "tasks:repo"));
}

#[test]
fn unknown_edges_cycles_and_pre_post_inversions_fail_before_launch() {
    let f = Fixture::new();
    f.write("hook.sh", "true");
    assert!(f
        .error("[tasks.bad]\nscript='hook.sh'\nafter=['files:missing']")
        .contains("unknown dependency files:missing"));
    assert!(f.error("[tasks.a]\nscript='hook.sh'\nafter=['tasks:b']\n[tasks.b]\nscript='hook.sh'\nafter=['tasks:a']").contains("dependency cycle"));
    assert!(f
        .error("[tasks.a]\nphase='pre'\nscript='hook.sh'\nafter=['packages:post']")
        .contains("pre task tasks:a depends on post"));
    assert!(f.error("[tasks.go]\ntemplate='install-toolchain-go'\n[tasks.code]\nphase='pre'\ntemplate='install-code-server'").contains("pre task tasks:code depends on post"));
}

#[test]
fn duplicate_and_overlapping_destinations_require_explicit_ordering() {
    let f = Fixture::new();
    f.write("data/file.txt", "hello");
    let definitions = "[files.a]\nsource='data'\ntarget='~/data'\n[files.b]\nsource='data/file.txt'\ntarget='~/data/file.txt'\n";
    assert!(f.error(definitions).contains("overlapping destinations"));
    f.compile(&format!("{definitions}after=['files:a']"))
        .unwrap();
    assert!(f.error("[files.a]\nsource='data'\ntarget='~/data'\n[files.b]\nsource='data'\ntarget='${beam.home}/data'").contains("duplicate destination"));
}

#[test]
fn remote_plan_contains_no_authoritative_local_paths_and_is_deterministic() {
    let f = Fixture::new();
    f.write("hook.sh", "true");
    let text = "[tasks.setup]\nscript='hook.sh'\n";
    let first = f.compile(text).unwrap();
    let second = f.compile(text).unwrap();
    let remote = serde_json::to_string(&first.plan).unwrap();
    assert!(!remote.contains(f.0.to_str().unwrap()));
    assert_eq!(remote, serde_json::to_string(&second.plan).unwrap());
    assert!(serde_json::to_string(&first)
        .unwrap()
        .contains(f.0.to_str().unwrap()));
}

#[test]
fn custom_templates_keep_source_origin_and_substitute_argv_without_shell_interpolation() {
    let f = Fixture::new();
    let script = f.write("base/hooks/install.sh", "true");
    f.write(
        "base/base.toml",
        r#"schema_version=1
[templates.install]
script = "hooks/install.sh"
args = ["--name", "${param.name}"]
packages = ["curl"]
[templates.install.env]
ENABLED = "${param.enabled}"
[templates.install.params.name]
kind = "string"
[templates.install.params.enabled]
kind = "boolean"
default = true
"#,
    );
    let compiled = f
        .compile(
            r#"extends=['base/base.toml']
[tasks.install]
template='user:install'
params={name='$(do-not-execute)'}
"#,
        )
        .unwrap();
    let Action::Script {
        artifact,
        args,
        env,
        ..
    } = &node(&compiled, "tasks:install").action
    else {
        panic!()
    };
    assert_eq!(source_path(&compiled.sources[artifact]), script);
    assert_eq!(args, &["--name", "$(do-not-execute)"]);
    assert_eq!(env["ENABLED"], "true");
    assert_eq!(packages(&compiled, Phase::Post), ["curl"]);
    assert!(f
        .error("extends=['base/base.toml']\n[tasks.install]\ntemplate='user:install'")
        .contains("missing custom parameter"));
    assert!(f.error("extends=['base/base.toml']\n[tasks.install]\ntemplate='user:install'\nparams={name=123}").contains("expected string"));
}

#[test]
fn remote_home_never_uses_workstation_home() {
    assert_eq!(plan::remote_path("~/a/../b").unwrap(), "${beam.home}/b");
    assert_eq!(plan::remote_path("/tmp/./x").unwrap(), "/tmp/x");
    assert!(plan::remote_path("../../foo").is_err());
    assert!(plan::remote_path("~/../../foo").is_err());
}

#[test]
fn shared_root_explanation_is_independent_of_task_id_order_and_post_trees_are_not_walked() {
    let f = Fixture::new();
    f.write("hooks/pre.sh", "true");
    f.write("hooks/post.sh", "true");
    f.write("data/file.txt", "hello");
    // An invalid descendant is a pack-time error. Offline compilation only
    // checks the declared root and must not scan a deferred tree.
    #[cfg(unix)]
    std::os::unix::fs::symlink("missing-file", f.0.join("data/dangling")).unwrap();
    let compiled = f
        .compile(
            r#"
[tasks.a-pre]
phase = "pre"
script = "hooks/pre.sh"
script_root = "hooks"
[tasks.z-post]
script = "hooks/post.sh"
script_root = "hooks"
[files.data]
source = "data"
target = "~/data"
"#,
        )
        .unwrap();
    assert_eq!(compiled.plan.artifacts.len(), 2);
    assert!(compiled.notes.iter().any(|n| n.contains("staged in pre")));
    assert!(depends(&compiled, "tasks:z-post", "artifact-ready:pre"));
}

#[test]
fn ssh_grants_cover_ssh_clones_or_named_tasks_and_stay_off_the_remote_plan() {
    let f = Fixture::new();
    let clones = "[tasks.core]\ntemplate='clone-git-repo'\nparams={repo='git@github.com:o/core.git', dest='~/core'}\n\
                  [tasks.web]\ntemplate='clone-git-repo'\nparams={repo='https://github.com/o/web', dest='~/web'}\n";
    let compiled = f.compile(&format!("{clones}[ssh_grant]\nhosts=['github.com']")).unwrap();
    let grant = compiled.ssh_grant.clone().unwrap();
    assert_eq!(grant.nodes, ["tasks:core"], "only the SSH clone");
    assert_eq!((grant.user.as_str(), grant.max_uses, grant.window_secs), ("git", 3, 600));
    assert_eq!(grant.phase, Phase::Post);
    assert_eq!(
        grant.task_runs("L"),
        ["L/launch:L:tasks:core"],
        "the tag bootstrap/src/launch/runner.rs gives the task"
    );
    let plan = serde_json::to_string(&compiled.plan).unwrap();
    assert!(!plan.contains("github.com\"]") && !plan.contains("ssh_grant"), "not in the Beam's plan");
    assert!(explain(&compiled).contains("SSH grant: github.com as git, 3 uses within 600s from interface-ready; covers tasks:core"));

    f.write("sync.sh", "git fetch\n");
    let named = f
        .compile(&format!(
            "{clones}[tasks.sync]\nscript='./sync.sh'\nphase='pre'\n[ssh_grant]\nhosts=['github.com']\ntasks=['sync','core']\nmax_uses=5\nwindow_secs=60"
        ))
        .unwrap()
        .ssh_grant
        .unwrap();
    assert_eq!(named.nodes, ["tasks:sync", "tasks:core"]);
    assert_eq!((named.max_uses, named.window_secs, named.phase), (5, 60, Phase::Pre), "a pre task opens it at acceptance");

    assert!(f.error(&format!("{clones}[ssh_grant]\nhosts=['github.com']\ntasks=['nope']")).contains("isn't an enabled task"));
    assert!(f.error("[ssh_grant]\nhosts=['github.com']").contains("covers no tasks"));
    assert!(f.error(&format!("{clones}[ssh_grant]\nhosts=['github.com']\nwindow_secs=7200")).contains("window_secs"));
    assert!(f.error(&format!("{clones}[ssh_grant]\nhosts=['github.com']\ntypo=1")).contains("unknown field"));

    // A child replaces an inherited grant whole; hosts = [] removes it.
    let base = f.write("base.toml", &format!("schema_version = 1\n{clones}[ssh_grant]\nhosts=['github.com']\n"));
    let child = |grant: &str| format!("extends=['{}']\n{grant}", base.display());
    assert!(f.compile(&child("")).unwrap().ssh_grant.is_some(), "inherited");
    assert!(f.compile(&child("[ssh_grant]\nhosts=[]")).unwrap().ssh_grant.is_none(), "removed");
    let replaced = f.compile(&child("[ssh_grant]\nhosts=['gitlab.com']\nuser='me'")).unwrap().ssh_grant.unwrap();
    assert_eq!((replaced.hosts, replaced.user), (vec!["gitlab.com".to_string()], "me".to_string()));
}
