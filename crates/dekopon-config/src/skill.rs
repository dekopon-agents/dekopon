//! Skills: `SKILL.md` directories an agent mounts as on-demand reference material.
//!
//! The on-disk format is the open Agent Skills layout — a directory named after the skill, a
//! `SKILL.md` whose YAML front matter carries the name and a one-line description, a Markdown body
//! of instructions, and optional supporting files beside it — so a skill written for another
//! client loads here unchanged. What this module adds is the reading discipline the rest of the
//! catalog already has: every bound is fixed before a byte is read, every problem names the file,
//! and a skill either loads whole or is refused.
//!
//! A skill is read once, at catalog load, into memory. The session layer that later shows it to a
//! model never touches the filesystem: it holds the text, which is what keeps the sandboxed shell's
//! "no filesystem" property true while a model can still read a reference document.
//!
//! Skill text is untrusted model text exactly as `instructions` is. It shapes answers and grants
//! nothing; nothing in a skill can widen a capability or name a principal.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use dekopon_core::{SkillId, SkillIdError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The file every skill directory must carry.
pub const SKILL_FILE_NAME: &str = "SKILL.md";
/// Maximum bytes of one `SKILL.md`, front matter included.
///
/// The format recommends keeping a body under a few hundred lines; a document past this bound is
/// context a model pays for on every turn after it reads it, and belongs in a resource file.
pub const MAX_SKILL_FILE_BYTES: usize = 64 * 1024;
/// Maximum bytes in a skill description, fixed by the format.
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 1024;
/// Maximum bytes of one supporting file.
///
/// The same ceiling a script's output and a textual chat attachment already have on the way into a
/// prompt, because a resource reaches the model by the same route.
pub const MAX_SKILL_RESOURCE_BYTES: usize = 256 * 1024;
/// Maximum supporting files one skill may carry.
pub const MAX_SKILL_RESOURCES: usize = 64;
/// Maximum bytes across every supporting file of one skill.
pub const MAX_SKILL_TOTAL_BYTES: usize = 1024 * 1024;
/// Maximum directory depth a resource may sit at beneath the skill directory.
pub const MAX_SKILL_RESOURCE_DEPTH: usize = 4;

/// One loaded skill: its front matter, its instructions, and every supporting file, in memory.
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

/// One supporting file inside a skill directory, read whole.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    /// Path relative to the skill directory, `/`-separated on every platform.
    pub path: String,
    /// The file's UTF-8 text.
    pub text: String,
}

impl Skill {
    /// The skill's name, equal to its directory name.
    #[must_use]
    pub fn name(&self) -> &SkillId {
        &self.name
    }

    /// The one-line description a model reads to decide whether the skill applies.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The Markdown instructions after the front matter.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Declared license, if the front matter named one.
    #[must_use]
    pub fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    /// Declared environment requirements, if the front matter named any.
    #[must_use]
    pub fn compatibility(&self) -> Option<&str> {
        self.compatibility.as_deref()
    }

    /// Free-form front-matter metadata, scalars rendered as text.
    #[must_use]
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The tools the author expected to be pre-approved.
    ///
    /// Recorded and rendered only. Dekopon's session has one scripting tool whatever a skill
    /// says, and authority comes from broker policy rather than from a file a model reads.
    #[must_use]
    pub fn allowed_tools(&self) -> Option<&str> {
        self.allowed_tools.as_deref()
    }

    /// Supporting files, sorted by relative path.
    #[must_use]
    pub fn resources(&self) -> &[SkillResource] {
        &self.resources
    }

    /// Looks up one supporting file by its relative path.
    #[must_use]
    pub fn resource(&self, path: &str) -> Option<&SkillResource> {
        self.resources
            .binary_search_by(|resource| resource.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.resources[index])
    }

    /// The directory the skill was read from.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
}

/// The front matter the Agent Skills format defines, and nothing else.
///
/// Strict, like every other authored document here: a misspelled key is a load failure naming it
/// rather than a field silently ignored.
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

/// Reads and validates one skill directory.
///
/// # Errors
///
/// Returns the first thing wrong with the directory, naming the file it was found in. A skill is
/// one authored unit, so unlike a catalog it is refused at the first problem rather than scanned
/// for all of them.
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

/// Splits `---` front matter from the Markdown body.
///
/// The opening fence must be the first line and the closing fence a line of its own. CRLF endings
/// are tolerated because a skill is frequently authored on another machine than the one that
/// loads it.
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

/// Walks the skill directory, reading every supporting file into memory.
///
/// Hidden entries are skipped because they are editor and version-control residue rather than
/// authored material; a symbolic link is refused rather than followed, because a skill is
/// authored content and a link is how one escapes the directory that was reviewed.
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

/// Reads one regular file as UTF-8 under a byte ceiling checked before the read.
fn read_bounded_text(path: &Path, maximum: usize) -> Result<String, SkillError> {
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
    // The metadata length is a hint a concurrent writer can outrun; the read itself is the bound.
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

/// Why one skill directory could not be loaded.
#[derive(Debug, Error)]
pub enum SkillError {
    /// The path is not a directory.
    #[error("skill path is not a directory: {path}")]
    NotADirectory {
        /// The refused path.
        path: PathBuf,
    },
    /// A file or directory could not be read.
    #[error("could not read skill file {path}")]
    Read {
        /// The path that failed.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A file exceeded its byte ceiling.
    #[error("skill file {path} is {length} bytes; the maximum is {maximum}")]
    TooLarge {
        /// The oversized file.
        path: PathBuf,
        /// Actual byte length.
        length: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// A file was not UTF-8 text.
    #[error("skill file {path} is not UTF-8 text")]
    NotUtf8 {
        /// The refused file.
        path: PathBuf,
        /// Where in the file the text stopped being UTF-8.
        #[source]
        source: std::string::FromUtf8Error,
    },
    /// A file name was not UTF-8, so the model could never name it in a `read_skill` call.
    #[error("skill file name {path} is not UTF-8")]
    NameNotUtf8 {
        /// The refused path.
        path: PathBuf,
    },
    /// A resource path did not resolve under the skill directory it was found in.
    #[error("skill file {path} is outside the skill directory it was read from")]
    OutsideRoot {
        /// The refused path.
        path: PathBuf,
        /// The prefix mismatch.
        #[source]
        source: std::path::StripPrefixError,
    },
    /// A symbolic link was found where authored content was expected.
    #[error("skill path {path} is a symbolic link, which a skill directory may not contain")]
    Symlink {
        /// The refused path.
        path: PathBuf,
    },
    /// Something that is neither a regular file nor a directory was found.
    #[error("skill path {path} is neither a regular file nor a directory")]
    NotRegular {
        /// The refused path.
        path: PathBuf,
    },
    /// Resources were nested deeper than the format allows here.
    #[error("skill directory {path} is nested deeper than {maximum} levels")]
    TooDeep {
        /// The directory past the limit.
        path: PathBuf,
        /// Maximum depth.
        maximum: usize,
    },
    /// The skill carries more supporting files than the ceiling.
    #[error("skill {path} has more than {maximum} supporting files")]
    TooManyResources {
        /// The skill directory.
        path: PathBuf,
        /// Maximum file count.
        maximum: usize,
    },
    /// The supporting files together exceed the ceiling.
    #[error("skill {path} has more than {maximum} bytes of supporting files")]
    ResourcesTooLarge {
        /// The skill directory.
        path: PathBuf,
        /// Maximum total bytes.
        maximum: usize,
    },
    /// `SKILL.md` does not open with a `---` front-matter fence.
    #[error("{path} must begin with YAML front matter between `---` lines")]
    MissingFrontMatter {
        /// The skill file.
        path: PathBuf,
    },
    /// The front matter never closed.
    #[error("{path} opens YAML front matter with `---` but never closes it")]
    UnterminatedFrontMatter {
        /// The skill file.
        path: PathBuf,
    },
    /// The front matter is not the strict shape the format defines.
    #[error("{path}: invalid skill front matter: {source}")]
    FrontMatter {
        /// The skill file.
        path: PathBuf,
        /// The decoder's diagnostic.
        #[source]
        source: serde_yaml::Error,
    },
    /// The front-matter name is outside the skill grammar.
    #[error("{path}: invalid skill name: {source}")]
    InvalidName {
        /// The skill file.
        path: PathBuf,
        /// The grammar violation.
        #[source]
        source: SkillIdError,
    },
    /// The front-matter name and the directory name disagree.
    #[error("{path}: skill name {name:?} must equal its directory name {directory:?}")]
    NameMismatch {
        /// The skill file.
        path: PathBuf,
        /// The authored name.
        name: String,
        /// The directory the skill lives in.
        directory: String,
    },
    /// The description is blank.
    #[error("{path}: skill description must not be empty")]
    EmptyDescription {
        /// The skill file.
        path: PathBuf,
    },
    /// The description is longer than the format allows.
    #[error("{path}: skill description is {length} bytes; the maximum is {maximum}")]
    DescriptionTooLong {
        /// The skill file.
        path: PathBuf,
        /// Actual byte length.
        length: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// A metadata value is a list or a map rather than a scalar.
    #[error("{path}: skill metadata {key:?} must be a scalar value")]
    MetadataValue {
        /// The skill file.
        path: PathBuf,
        /// The offending key.
        key: String,
    },
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
