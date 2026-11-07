use super::{
    plan,
    profile::{embedded_asset, Source},
};
use anyhow::{bail, Context};
use beams_artifacts::{read_json, write_json, TemporaryDirectory};
use beams_protocol::launch::{BundleDescriptor, Compression, Declaration, Phase};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalInputs {
    compression: Compression,
    sources: BTreeMap<String, Source>,
}

pub fn prepare(
    profile: &str,
    output: &Path,
    compression: Compression,
) -> anyhow::Result<Declaration> {
    if std::fs::symlink_metadata(output).is_ok() {
        bail!("launch output already exists: {}", output.display());
    }
    let compilation = plan::compile(profile)?;
    let ssh_grant = compilation.ssh_grant.clone();
    let declaration = beams_artifacts::declaration(compilation.plan)?;
    let temp = TemporaryDirectory::new(
        output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    write_json(&temp.0.join("launch.json"), &declaration)?;
    let local = LocalInputs {
        compression,
        sources: compilation.sources,
    };
    // Workstation-only, kept separate from the files that will be uploaded.
    write_json(&temp.0.join("local-inputs.json"), &local)?;
    if let Some(grant) = &ssh_grant {
        write_json(&temp.0.join(crate::startup::LaunchGrant::FILE), grant)?;
    }
    phase(&temp.0, &declaration, &local, Phase::Pre)?;
    temp.publish(output)?;
    Ok(declaration)
}

pub fn prepare_post(output: &Path) -> anyhow::Result<BundleDescriptor> {
    let declaration: Declaration = read_json(&output.join("launch.json"))?;
    let local: LocalInputs = read_json(&output.join("local-inputs.json"))?;
    beams_artifacts::validate_declaration(&declaration)?;
    phase(output, &declaration, &local, Phase::Post)
}

fn phase(
    output: &Path,
    declaration: &Declaration,
    local: &LocalInputs,
    phase: Phase,
) -> anyhow::Result<BundleDescriptor> {
    let destination = output.join(phase.name());
    let filename = match local.compression {
        Compression::None => "bundle.tar",
        Compression::Gzip => "bundle.tar.gz",
    };
    if std::fs::symlink_metadata(&destination).is_ok() {
        let descriptor: BundleDescriptor = read_json(&destination.join("descriptor.json"))?;
        if descriptor.launch_id != declaration.launch_id
            || descriptor.plan_digest != declaration.plan_digest
            || descriptor.phase != phase
        {
            bail!("saved bundle does not match launch declaration");
        }
        if beams_artifacts::hash_file(&destination.join(filename))?
            != (descriptor.sha256.clone(), descriptor.bytes)
        {
            bail!("saved spool changed; refusing to replace this launch's frozen inputs");
        }
        return Ok(descriptor);
    }
    let sources = local
        .sources
        .iter()
        .filter(|(id, _)| {
            declaration
                .plan
                .artifacts
                .iter()
                .any(|a| &a.id == *id && a.phase == phase)
        })
        .map(|(id, source)| {
            let selection = match source {
                Source::Local(path) => beams_artifacts::Selection::Local(path.clone()),
                Source::Embedded(name) => beams_artifacts::Selection::Embedded {
                    name: name.clone(),
                    bytes: embedded_asset(name)
                        .with_context(|| format!("unknown embedded asset {name}"))?,
                },
            };
            Ok((id.clone(), selection))
        })
        .collect::<anyhow::Result<_>>()?;
    let temp = TemporaryDirectory::new(output)?;
    let descriptor = beams_artifacts::pack(
        declaration,
        phase,
        &sources,
        &temp.0.join(filename),
        local.compression,
    )?;
    write_json(&temp.0.join("descriptor.json"), &descriptor)?;
    temp.publish(&destination)?;
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparing_pre_defers_post_sources_and_later_freezes_post_for_replay() {
        let temp = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        std::fs::create_dir(temp.0.join("data")).unwrap();
        std::fs::write(temp.0.join("data/file"), "first").unwrap();
        std::fs::write(
            temp.0.join("environment.toml"),
            "schema_version=1\n[files.data]\nsource='data'\ntarget='~/data'",
        )
        .unwrap();
        let output = temp.0.join("launch");
        prepare(
            temp.0.join("environment.toml").to_str().unwrap(),
            &output,
            Compression::Gzip,
        )
        .unwrap();
        assert!(!output.join("post").exists());
        assert!(prepare(
            temp.0.join("environment.toml").to_str().unwrap(),
            &output,
            Compression::Gzip
        )
        .is_err());
        std::fs::write(temp.0.join("data/file"), "second").unwrap();
        let first = prepare_post(&output).unwrap();
        std::fs::write(temp.0.join("data/file"), "third").unwrap();
        let second = prepare_post(&output).unwrap();
        assert_eq!(first.sha256, second.sha256);
        let stage = temp.0.join("stage");
        let manifest =
            beams_artifacts::receive(&output.join("post/bundle.tar.gz"), &second, &stage).unwrap();
        assert_eq!(
            std::fs::read_to_string(stage.join(&manifest.artifacts[0].root).join("file")).unwrap(),
            "second"
        );
    }
}
