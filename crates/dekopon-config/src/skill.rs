//! Skill text is untrusted, like model instructions; it can shape answers but never widen a
//! capability or name a principal.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use dekopon_core::{SkillId, SkillIdError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const MAX_SKILL_FILE_BYTES: usize = 64 * 1024;
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_SKILL_RESOURCE_BYTES: usize = 256 * 1024;
pub const MAX_SKILL_RESOURCES: usize = 64;
pub const MAX_SKILL_TOTAL_BYTES: usize = 1024 * 1024;
pub const MAX_SKILL_RESOURCE_DEPTH: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    name: SkillId,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_tools: Option<String>,
    body: String,
    resources: Vec<SkillResource>,
    source: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    pub path: String,
    pub text: String,
}

impl Skill {
    #[must_use]
    pub fn name(&self) -> &SkillId {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    #[must_use]
    pub fn compatibility(&self) -> Option<&str> {
        self.compatibility.as_deref()
    }

    #[must_use]
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// Recorded as metadata only; real tool authority always comes from broker policy, never from
    /// what a skill file declares.
    #[must_use]
    pub fn allowed_tools(&self) -> Option<&str> {
        self.allowed_tools.as_deref()
    }

    /// Sorted by relative path; resource() binary-searches this list and depends on that order
    /// being maintained.
    #[must_use]
    pub fn resources(&self) -> &[SkillResource] {
        &self.resources
    }

    #[must_use]
    pub fn resource(&self, path: &str) -> Option<&SkillResource> {
        self.resources
            .binary_search_by(|resource| resource.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.resources[index])
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct FrontMatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    allowed_tools: Option<String>,
}

pub fn load_skill(directory: impl AsRef<Path>) -> Result<Skill, SkillError> {
    let directory = directory.as_ref();
    let metadata = fs::symlink_metadata(directory).map_err(|source| SkillError::Read {
        path: directory.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SkillError::Symlink {
            path: directory.to_path_buf(),
        });
    }
    if !metadata.is_dir() {
        return Err(SkillError::NotADirectory {
            path: directory.to_path_buf(),
        });
    }
    let directory_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SkillError::NotADirectory {
            path: directory.to_path_buf(),
        })?;

    let skill_file = directory.join(SKILL_FILE_NAME);
    let text = read_bounded_text(&skill_file, MAX_SKILL_FILE_BYTES)?;
    let (front_matter, body) = split_front_matter(&text, &skill_file)?;
    let front_matter = serde_yaml::from_str::<FrontMatter>(front_matter).map_err(|source| {
        SkillError::FrontMatter {
            path: skill_file.clone(),
            source,
        }
    })?;
    let name = front_matter
        .name
        .parse::<SkillId>()
        .map_err(|source| SkillError::InvalidName {
            path: skill_file.clone(),
            source,
        })?;
    if name.as_str() != directory_name {
        return Err(SkillError::NameMismatch {
            path: skill_file,
            name: name.to_string(),
            directory: directory_name.to_owned(),
        });
    }
    let description = front_matter.description.trim().to_owned();
    if description.is_empty() {
        return Err(SkillError::EmptyDescription { path: skill_file });
    }
    if description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        return Err(SkillError::DescriptionTooLong {
            path: skill_file,
            length: description.len(),
            maximum: MAX_SKILL_DESCRIPTION_BYTES,
        });
    }
    let mut metadata = BTreeMap::new();
    for (key, value) in front_matter.metadata {
        let rendered = match value {
            serde_yaml::Value::String(text) => text,
            serde_yaml::Value::Bool(flag) => flag.to_string(),
            serde_yaml::Value::Number(number) => number.to_string(),
            serde_yaml::Value::Null => String::new(),
            serde_yaml::Value::Sequence(_) | serde_yaml::Value::Mapping(_) => {
                return Err(SkillError::MetadataValue {
                    path: skill_file,
                    key,
                });
            }
            serde_yaml::Value::Tagged(_) => {
                return Err(SkillError::MetadataValue {
                    path: skill_file,
                    key,
                });
            }
        };
        metadata.insert(key, rendered);
    }

    let mut resources = Vec::new();
    let mut total_bytes = 0_usize;
    collect_resources(directory, directory, 0, &mut resources, &mut total_bytes)?;
    resources.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(Skill {
        name,
        description,
        license: front_matter
            .license
            .filter(|value| !value.trim().is_empty()),
        compatibility: front_matter
            .compatibility
            .filter(|value| !value.trim().is_empty()),
        metadata,
        allowed_tools: front_matter
            .allowed_tools
            .filter(|value| !value.trim().is_empty()),
        body: body.trim().to_owned(),
        resources,
        source: directory.to_path_buf(),
    })
}

fn split_front_matter<'a>(text: &'a str, path: &Path) -> Result<(&'a str, &'a str), SkillError> {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Err(SkillError::MissingFrontMatter {
            path: path.to_path_buf(),
        });
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(SkillError::MissingFrontMatter {
            path: path.to_path_buf(),
        });
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let front_matter = &text[first.len()..offset];
            let body = &text[offset + line.len()..];
            return Ok((front_matter, body));
        }
        offset += line.len();
    }
    Err(SkillError::UnterminatedFrontMatter {
        path: path.to_path_buf(),
    })
}

/// Symlinks are refused rather than followed, since a link is how content would escape the
/// directory that was reviewed.
fn collect_resources(
    root: &Path,
    directory: &Path,
    depth: usize,
    resources: &mut Vec<SkillResource>,
    total_bytes: &mut usize,
) -> Result<(), SkillError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| SkillError::Read {
            path: directory.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SkillError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(SkillError::NameNotUtf8 { path });
        };
        if name.starts_with('.') {
            continue;
        }
        let metadata = entry.metadata().map_err(|source| SkillError::Read {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(SkillError::Symlink { path });
        }
        if metadata.is_dir() {
            if depth + 1 > MAX_SKILL_RESOURCE_DEPTH {
                return Err(SkillError::TooDeep {
                    path,
                    maximum: MAX_SKILL_RESOURCE_DEPTH,
                });
            }
            collect_resources(root, &path, depth + 1, resources, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(SkillError::NotRegular { path });
        }
        if depth == 0 && name == SKILL_FILE_NAME {
            continue;
        }
        if resources.len() == MAX_SKILL_RESOURCES {
            return Err(SkillError::TooManyResources {
                path: root.to_path_buf(),
                maximum: MAX_SKILL_RESOURCES,
            });
        }
        let text = read_bounded_text(&path, MAX_SKILL_RESOURCE_BYTES)?;
        *total_bytes = total_bytes.saturating_add(text.len());
        if *total_bytes > MAX_SKILL_TOTAL_BYTES {
            return Err(SkillError::ResourcesTooLarge {
                path: root.to_path_buf(),
                maximum: MAX_SKILL_TOTAL_BYTES,
            });
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|source| SkillError::OutsideRoot {
                path: path.clone(),
                source,
            })?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        resources.push(SkillResource {
            path: relative,
            text,
        });
    }
    Ok(())
}

pub(crate) fn read_bounded_text(path: &Path, maximum: usize) -> Result<String, SkillError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| SkillError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SkillError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(SkillError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    let length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if length > maximum {
        return Err(SkillError::TooLarge {
            path: path.to_path_buf(),
            length,
            maximum,
        });
    }
    let bytes = fs::read(path).map_err(|source| SkillError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    // The metadata length is only a hint a concurrent writer can outrun; the actual read length is
    // what's enforced.
    if bytes.len() > maximum {
        return Err(SkillError::TooLarge {
            path: path.to_path_buf(),
            length: bytes.len(),
            maximum,
        });
    }
    String::from_utf8(bytes).map_err(|source| SkillError::NotUtf8 {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("skill path is not a directory: {path}")]
    NotADirectory { path: PathBuf },
    #[error("could not read skill file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("skill file {path} is {length} bytes; the maximum is {maximum}")]
    TooLarge {
        path: PathBuf,
        length: usize,
        maximum: usize,
    },
    #[error("skill file {path} is not UTF-8 text")]
    NotUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("skill file name {path} is not UTF-8")]
    NameNotUtf8 { path: PathBuf },
    #[error("skill file {path} is outside the skill directory it was read from")]
    OutsideRoot {
        path: PathBuf,
        #[source]
        source: std::path::StripPrefixError,
    },
    #[error("skill path {path} is a symbolic link, which a skill directory may not contain")]
    Symlink { path: PathBuf },
    #[error("skill path {path} is neither a regular file nor a directory")]
    NotRegular { path: PathBuf },
    #[error("skill directory {path} is nested deeper than {maximum} levels")]
    TooDeep { path: PathBuf, maximum: usize },
    #[error("skill {path} has more than {maximum} supporting files")]
    TooManyResources { path: PathBuf, maximum: usize },
    #[error("skill {path} has more than {maximum} bytes of supporting files")]
    ResourcesTooLarge { path: PathBuf, maximum: usize },
    #[error("{path} must begin with YAML front matter between `---` lines")]
    MissingFrontMatter { path: PathBuf },
    #[error("{path} opens YAML front matter with `---` but never closes it")]
    UnterminatedFrontMatter { path: PathBuf },
    #[error("{path}: invalid skill front matter: {source}")]
    FrontMatter {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{path}: invalid skill name: {source}")]
    InvalidName {
        path: PathBuf,
        #[source]
        source: SkillIdError,
    },
    #[error("{path}: skill name {name:?} must equal its directory name {directory:?}")]
    NameMismatch {
        path: PathBuf,
        name: String,
        directory: String,
    },
    #[error("{path}: skill description must not be empty")]
    EmptyDescription { path: PathBuf },
    #[error("{path}: skill description is {length} bytes; the maximum is {maximum}")]
    DescriptionTooLong {
        path: PathBuf,
        length: usize,
        maximum: usize,
    },
    #[error("{path}: skill metadata {key:?} must be a scalar value")]
    MetadataValue { path: PathBuf, key: String },
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        MAX_SKILL_DESCRIPTION_BYTES, MAX_SKILL_FILE_BYTES, MAX_SKILL_RESOURCES, SKILL_FILE_NAME,
        SkillError, load_skill,
    };

    const SKILL: &str = "---\nname: pull-request-review\ndescription: Use when reviewing a pull request; covers what to read and how to comment.\nlicense: MIT\nmetadata:\n  author: dekopon\n  version: 2\n---\n\n# Pull request review\n\nRead the diff before commenting.\n";

    fn write_skill(root: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).expect("skill directory");
        fs::write(directory.join(SKILL_FILE_NAME), contents).expect("skill file");
        directory
    }

    #[test]
    fn loads_front_matter_body_and_sorted_resources() {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = write_skill(root.path(), "pull-request-review", SKILL);
        fs::create_dir_all(directory.join("references")).expect("references directory");
        fs::write(
            directory.join("references/checklist.md"),
            "- read the tests\n",
        )
        .expect("resource");
        fs::write(directory.join("README.md"), "about\n").expect("resource");
        fs::write(directory.join(".hidden"), "ignored\n").expect("hidden file");

        let skill = load_skill(&directory).expect("valid skill loads");

        assert_eq!(skill.name().as_str(), "pull-request-review");
        assert_eq!(
            skill.description(),
            "Use when reviewing a pull request; covers what to read and how to comment."
        );
        assert_eq!(skill.license(), Some("MIT"));
        assert_eq!(skill.metadata()["author"], "dekopon");
        assert_eq!(skill.metadata()["version"], "2");
        assert!(skill.body().starts_with("# Pull request review"));
        assert_eq!(
            skill
                .resources()
                .iter()
                .map(|resource| resource.path.as_str())
                .collect::<Vec<_>>(),
            ["README.md", "references/checklist.md"]
        );
        assert_eq!(
            skill
                .resource("references/checklist.md")
                .map(|resource| resource.text.as_str()),
            Some("- read the tests\n")
        );
        assert!(
            skill.resource("SKILL.md").is_none(),
            "the skill file is not a resource"
        );
        assert!(skill.resource(".hidden").is_none());
        assert_eq!(skill.source(), directory);
    }

    #[test]
    fn the_name_must_match_the_directory() {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = write_skill(root.path(), "renamed", SKILL);

        let error = load_skill(&directory).expect_err("a mismatched name is refused");
        assert!(
            matches!(&error, SkillError::NameMismatch { name, directory, .. }
                if name == "pull-request-review" && directory == "renamed"),
            "{error}"
        );
    }

    #[test]
    fn front_matter_is_strict_and_bounded() {
        let root = tempfile::tempdir().expect("temporary directory");

        let unknown = write_skill(
            root.path(),
            "unknown-key",
            "---\nname: unknown-key\ndescription: x\nauthor: me\n---\nbody\n",
        );
        assert!(matches!(
            load_skill(&unknown),
            Err(SkillError::FrontMatter { .. })
        ));

        let missing = write_skill(root.path(), "no-front-matter", "# just markdown\n");
        assert!(matches!(
            load_skill(&missing),
            Err(SkillError::MissingFrontMatter { .. })
        ));

        let open = write_skill(root.path(), "open", "---\nname: open\ndescription: x\n");
        assert!(matches!(
            load_skill(&open),
            Err(SkillError::UnterminatedFrontMatter { .. })
        ));

        let blank = write_skill(
            root.path(),
            "blank",
            "---\nname: blank\ndescription: '  '\n---\n",
        );
        assert!(matches!(
            load_skill(&blank),
            Err(SkillError::EmptyDescription { .. })
        ));

        let long = format!(
            "---\nname: long\ndescription: {}\n---\n",
            "d".repeat(MAX_SKILL_DESCRIPTION_BYTES + 1)
        );
        let long = write_skill(root.path(), "long", &long);
        assert!(matches!(
            load_skill(&long),
            Err(SkillError::DescriptionTooLong { .. })
        ));

        let bad_name = write_skill(
            root.path(),
            "Bad_Name",
            "---\nname: Bad_Name\ndescription: x\n---\n",
        );
        assert!(matches!(
            load_skill(&bad_name),
            Err(SkillError::InvalidName { .. })
        ));

        let nested = write_skill(
            root.path(),
            "nested",
            "---\nname: nested\ndescription: x\nmetadata:\n  tags: [a, b]\n---\n",
        );
        assert!(matches!(
            load_skill(&nested),
            Err(SkillError::MetadataValue { key, .. }) if key == "tags"
        ));
    }

    #[test]
    fn oversized_and_non_text_files_are_refused_before_they_reach_a_prompt() {
        let root = tempfile::tempdir().expect("temporary directory");
        let huge = format!(
            "---\nname: huge\ndescription: x\n---\n{}",
            "x".repeat(MAX_SKILL_FILE_BYTES)
        );
        let huge = write_skill(root.path(), "huge", &huge);
        assert!(matches!(
            load_skill(&huge),
            Err(SkillError::TooLarge { .. })
        ));

        let binary = write_skill(
            root.path(),
            "binary",
            "---\nname: binary\ndescription: x\n---\n",
        );
        fs::write(binary.join("blob.bin"), [0xff, 0xfe, 0x00]).expect("binary resource");
        assert!(matches!(
            load_skill(&binary),
            Err(SkillError::NotUtf8 { .. })
        ));

        let many = write_skill(
            root.path(),
            "many",
            "---\nname: many\ndescription: x\n---\n",
        );
        for index in 0..=MAX_SKILL_RESOURCES {
            fs::write(many.join(format!("file-{index:03}.md")), "x").expect("resource");
        }
        assert!(matches!(
            load_skill(&many),
            Err(SkillError::TooManyResources { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_refused_rather_than_followed() {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = write_skill(
            root.path(),
            "linked",
            "---\nname: linked\ndescription: x\n---\n",
        );
        let outside = root.path().join("outside.md");
        fs::write(&outside, "secret\n").expect("outside file");
        std::os::unix::fs::symlink(&outside, directory.join("escape.md")).expect("symlink");

        assert!(matches!(
            load_skill(&directory),
            Err(SkillError::Symlink { .. })
        ));
    }

    #[test]
    fn a_missing_directory_names_itself() {
        let root = tempfile::tempdir().expect("temporary directory");
        let error = load_skill(root.path().join("absent")).expect_err("absent skill is refused");
        assert!(matches!(&error, SkillError::Read { path, .. } if path.ends_with("absent")));

        let file = root.path().join("file");
        fs::write(&file, "x").expect("plain file");
        assert!(matches!(
            load_skill(&file),
            Err(SkillError::NotADirectory { .. })
        ));
    }

    #[test]
    fn crlf_front_matter_loads() {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = write_skill(
            root.path(),
            "crlf",
            "---\r\nname: crlf\r\ndescription: Windows authored\r\n---\r\nbody\r\n",
        );
        let skill = load_skill(&directory).expect("CRLF skill loads");
        assert_eq!(skill.description(), "Windows authored");
        assert_eq!(skill.body(), "body");
    }
}
