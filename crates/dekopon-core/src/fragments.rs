use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use serde_yaml::{Mapping, Value};
use thiserror::Error;

/// How a configuration directory's fragments combine. Keys in `merged_by_name` are maps whose
/// entries union by name, keys in `concatenated` are lists that append, and every other key is set
/// by exactly one fragment, so no fragment overrides another and file order never changes the
/// result.
pub struct MergeRules {
    pub merged_by_name: &'static [&'static str],
    pub concatenated: &'static [&'static str],
}

#[derive(Debug, Eq, PartialEq)]
pub struct FragmentConflict {
    pub key: String,
    pub files: Vec<PathBuf>,
}

impl fmt::Display for FragmentConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let files = self
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        write!(formatter, "{} is set in {files}", self.key)
    }
}

#[derive(Debug, Error)]
pub enum FragmentError {
    #[error(
        "configuration directory {path} must be owned by the daemon and not group or world writable"
    )]
    InsecureDirectory { path: PathBuf },
    #[error("could not read configuration directory {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configuration fragments conflict: {}", render(.conflicts))]
    Conflicts { conflicts: Vec<FragmentConflict> },
    #[error("configuration fragments disagree on apiVersion")]
    MixedApiVersions,
}

fn render(conflicts: &[FragmentConflict]) -> String {
    conflicts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn scan_directory(
    directory: &Path,
    expected_uid: u32,
    extension: &str,
) -> Result<Vec<PathBuf>, FragmentError> {
    let read = |source| FragmentError::Read {
        path: directory.to_path_buf(),
        source,
    };
    let metadata = std::fs::symlink_metadata(directory).map_err(read)?;
    if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o022 != 0 {
        return Err(FragmentError::InsecureDirectory {
            path: directory.to_path_buf(),
        });
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(read)? {
        let path = entry.map_err(read)?.path();
        if path
            .extension()
            .is_some_and(|candidate| candidate == extension)
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn merge(
    fragments: Vec<(PathBuf, Mapping)>,
    rules: &MergeRules,
) -> Result<Mapping, FragmentError> {
    let mut merged = Mapping::new();
    let mut owners = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut versions = BTreeSet::new();
    for (path, fragment) in fragments {
        for (key, value) in fragment {
            let name = key.as_str().unwrap_or_default().to_owned();
            if name == "apiVersion" {
                versions.insert(serde_yaml::to_string(&value).unwrap_or_default());
                merged.insert(key, value);
                continue;
            }
            let by_name = rules.merged_by_name.contains(&name.as_str());
            let collection = by_name || rules.concatenated.contains(&name.as_str());
            match (merged.get_mut(&key), value) {
                (Some(Value::Sequence(into)), Value::Sequence(from)) if collection => {
                    into.extend(from);
                }
                (Some(Value::Mapping(into)), Value::Mapping(from)) if by_name => {
                    for (entry, entry_value) in from {
                        let owner = format!("{name}.{}", entry.as_str().unwrap_or_default());
                        owners.entry(owner).or_default().push(path.clone());
                        into.insert(entry, entry_value);
                    }
                }
                (None, value) => {
                    if let (true, Value::Mapping(entries)) = (by_name, &value) {
                        for entry in entries.keys() {
                            let owner = format!("{name}.{}", entry.as_str().unwrap_or_default());
                            owners.entry(owner).or_default().push(path.clone());
                        }
                    }
                    owners.entry(name).or_default().push(path.clone());
                    merged.insert(key, value);
                }
                (Some(_), _) => {
                    owners.entry(name).or_default().push(path.clone());
                }
            }
        }
    }
    let conflicts = owners
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(key, files)| FragmentConflict { key, files })
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Err(FragmentError::Conflicts { conflicts });
    }
    if versions.len() > 1 {
        return Err(FragmentError::MixedApiVersions);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FragmentError, MergeRules, merge};

    const RULES: MergeRules = MergeRules {
        merged_by_name: &["principals"],
        concatenated: &["identities"],
    };

    fn fragment(name: &str, yaml: &str) -> (PathBuf, serde_yaml::Mapping) {
        (
            PathBuf::from(name),
            serde_yaml::from_str(yaml).expect("fragment parses"),
        )
    }

    #[test]
    fn collections_union_and_every_collision_is_reported_together() {
        let merged = merge(
            vec![
                fragment(
                    "a.yaml",
                    "principals: { isaac: {} }\nidentities: [1]\nsocketPath: /a",
                ),
                fragment("b.yaml", "principals: { simon: {} }\nidentities: [2]"),
            ],
            &RULES,
        )
        .expect("disjoint fragments merge");
        assert_eq!(
            merged["principals"]
                .as_mapping()
                .map(serde_yaml::Mapping::len),
            Some(2)
        );
        assert_eq!(merged["identities"].as_sequence().map(Vec::len), Some(2));

        let Err(FragmentError::Conflicts { conflicts }) = merge(
            vec![
                fragment("a.yaml", "principals: { isaac: {} }\nsocketPath: /a"),
                fragment("b.yaml", "principals: { isaac: {} }\nsocketPath: /b"),
                fragment("c.yaml", "identities: [1]"),
                fragment("d.yaml", "identities: {}"),
            ],
            &RULES,
        ) else {
            panic!("colliding fragments must not merge");
        };
        let keys = conflicts
            .iter()
            .map(|conflict| conflict.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["identities", "principals.isaac", "socketPath"]);
    }

    #[test]
    fn the_result_does_not_depend_on_file_order() {
        let forward = merge(
            vec![
                fragment("a.yaml", "principals: { isaac: {} }"),
                fragment("b.yaml", "principals: { simon: {} }"),
            ],
            &RULES,
        )
        .expect("merges");
        let backward = merge(
            vec![
                fragment("b.yaml", "principals: { simon: {} }"),
                fragment("a.yaml", "principals: { isaac: {} }"),
            ],
            &RULES,
        )
        .expect("merges");
        let names = |merged: &serde_yaml::Mapping| {
            let mut names = merged["principals"]
                .as_mapping()
                .expect("map")
                .keys()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&forward), names(&backward));
    }
}
