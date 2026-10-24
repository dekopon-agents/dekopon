use std::collections::BTreeMap;

use dekopon_core::{
    MAX_SKILL_FILE_BYTES, MAX_SKILL_RESOURCE_BYTES, MAX_SKILL_RESOURCE_DEPTH, MAX_SKILL_RESOURCES,
    MAX_SKILL_TOTAL_BYTES, ProviderId, SKILL_FILE_NAME, SkillId, SkillIdError,
};
use thiserror::Error;
use wasmparser::{Encoding, Parser, Payload};

#[derive(Debug, Eq, PartialEq)]
pub struct SkillFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum SkillReadError {
    #[error("malformed Wasm")]
    Wasm(#[from] wasmparser::BinaryReaderError),
    #[error("provider artifact must be a component")]
    NotComponent,
    #[error("skill section path must contain a skill and file: {path}")]
    MissingFilePath { path: String },
    #[error("empty path segment: {path}")]
    EmptySegment { path: String },
    #[error("dot path segment: {path}")]
    DotSegment { path: String },
    #[error("parent path segment: {path}")]
    ParentSegment { path: String },
    #[error("hidden path segment: {path}")]
    HiddenSegment { path: String },
    #[error("invalid filesystem path segment: {path}")]
    InvalidSegment { path: String },
    #[error("invalid skill name: {skill}")]
    InvalidSkill {
        skill: String,
        #[source]
        source: SkillIdError,
    },
    #[error("skill does not belong to the provider: {skill}")]
    ProviderMismatch { skill: String },
    #[error("skill file exceeds its byte bound: {path}")]
    SkillFileTooLarge { path: String },
    #[error("resource exceeds its byte bound: {path}")]
    ResourceTooLarge { path: String },
    #[error("skill exceeds its resource count bound: {skill}")]
    TooManyResources { skill: String },
    #[error("skill exceeds its total resource byte bound: {skill}")]
    ResourcesTooLarge { skill: String },
    #[error("path exceeds the resource directory depth bound: {path}")]
    TooDeep { path: String },
    #[error("skill file is not UTF-8: {path}")]
    NotUtf8 {
        path: String,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("duplicate skill path: {path}")]
    DuplicatePath { path: String },
    #[error("skill path is also a directory: {path}")]
    PathIsDirectory { path: String },
    #[error("skill has no SKILL.md: {skill}")]
    MissingSkillFile { skill: String },
}

pub fn read(provider: &ProviderId, component: &[u8]) -> Result<Vec<SkillFile>, SkillReadError> {
    let mut files = BTreeMap::new();
    let mut skills = BTreeMap::<&str, (usize, usize)>::new();
    for payload in Parser::new(0).parse_all(component) {
        let payload = payload?;
        if let Payload::Version {
            encoding, range, ..
        } = &payload
            && range.start == 0
            && *encoding != Encoding::Component
        {
            return Err(SkillReadError::NotComponent);
        }
        let Payload::CustomSection(section) = payload else {
            continue;
        };
        let Some(path) = section.name().strip_prefix("dekopon:skill/") else {
            continue;
        };
        let (skill, relative) = validate_path(provider, path)?;
        let bytes = section.data();
        let totals = skills.entry(skill).or_default();
        if relative == SKILL_FILE_NAME {
            if bytes.len() > MAX_SKILL_FILE_BYTES {
                return Err(SkillReadError::SkillFileTooLarge { path: path.into() });
            }
        } else {
            if bytes.len() > MAX_SKILL_RESOURCE_BYTES {
                return Err(SkillReadError::ResourceTooLarge { path: path.into() });
            }
            totals.0 += 1;
            totals.1 += bytes.len();
            if totals.0 > MAX_SKILL_RESOURCES {
                return Err(SkillReadError::TooManyResources {
                    skill: skill.into(),
                });
            }
            if totals.1 > MAX_SKILL_TOTAL_BYTES {
                return Err(SkillReadError::ResourcesTooLarge {
                    skill: skill.into(),
                });
            }
        }
        std::str::from_utf8(bytes).map_err(|source| SkillReadError::NotUtf8 {
            path: path.into(),
            source,
        })?;
        if files.insert(path, bytes).is_some() {
            return Err(SkillReadError::DuplicatePath { path: path.into() });
        }
    }
    for path in files.keys() {
        for (index, _) in path.match_indices('/') {
            if files.contains_key(&path[..index]) {
                return Err(SkillReadError::PathIsDirectory {
                    path: path[..index].into(),
                });
            }
        }
    }
    for skill in skills.keys() {
        if !files.contains_key(format!("{skill}/{SKILL_FILE_NAME}").as_str()) {
            return Err(SkillReadError::MissingSkillFile {
                skill: (*skill).into(),
            });
        }
    }
    Ok(files
        .into_iter()
        .map(|(path, bytes)| SkillFile {
            path: path.into(),
            bytes: bytes.into(),
        })
        .collect())
}

fn validate_path<'a>(
    provider: &ProviderId,
    path: &'a str,
) -> Result<(&'a str, &'a str), SkillReadError> {
    for segment in path.split('/') {
        let error = match segment {
            "" => Some(SkillReadError::EmptySegment { path: path.into() }),
            "." => Some(SkillReadError::DotSegment { path: path.into() }),
            ".." => Some(SkillReadError::ParentSegment { path: path.into() }),
            _ if segment.starts_with('.') => {
                Some(SkillReadError::HiddenSegment { path: path.into() })
            }
            _ if segment.contains('\0') => {
                Some(SkillReadError::InvalidSegment { path: path.into() })
            }
            _ => None,
        };
        if let Some(error) = error {
            return Err(error);
        }
    }
    let (skill, relative) = path
        .split_once('/')
        .ok_or_else(|| SkillReadError::MissingFilePath { path: path.into() })?;
    skill
        .parse::<SkillId>()
        .map_err(|source| SkillReadError::InvalidSkill {
            skill: skill.into(),
            source,
        })?;
    if skill != provider.as_str()
        && !skill
            .strip_prefix(provider.as_str())
            .is_some_and(|suffix| suffix.starts_with('-'))
    {
        return Err(SkillReadError::ProviderMismatch {
            skill: skill.into(),
        });
    }
    if relative.split('/').count() - 1 > MAX_SKILL_RESOURCE_DEPTH {
        return Err(SkillReadError::TooDeep { path: path.into() });
    }
    Ok((skill, relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use wasm_encoder::{Component, CustomSection, Module, ModuleSection, NestedComponentSection};

    fn section<'a>(path: &str, bytes: &'a [u8]) -> CustomSection<'a> {
        CustomSection {
            name: Cow::Owned(format!("dekopon:skill/{path}")),
            data: Cow::Borrowed(bytes),
        }
    }

    fn component(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut component = Component::new();
        for (path, bytes) in files {
            component.section(&section(path, bytes));
        }
        component.finish()
    }

    fn read_files(files: &[(&str, &[u8])]) -> Result<Vec<SkillFile>, SkillReadError> {
        read(&"gh".parse().unwrap(), &component(files))
    }

    #[test]
    fn no_sections_means_no_skills() {
        assert!(read_files(&[]).unwrap().is_empty());
    }

    #[test]
    fn files_at_every_nesting_level_are_returned_verbatim() {
        let mut module = Module::new();
        module.section(&section("gh/references/core.md", b"core\r\n"));
        let mut nested = Component::new();
        nested.section(&section("gh/nested.md", b"nested\n"));
        nested.section(&ModuleSection(&module));
        let mut root = Component::new();
        root.section(&section("gh/SKILL.md", b"root"));
        root.section(&NestedComponentSection(&nested));
        assert_eq!(
            read(&"gh".parse().unwrap(), &root.finish()).unwrap(),
            vec![
                SkillFile {
                    path: "gh/SKILL.md".into(),
                    bytes: b"root".to_vec()
                },
                SkillFile {
                    path: "gh/nested.md".into(),
                    bytes: b"nested\n".to_vec()
                },
                SkillFile {
                    path: "gh/references/core.md".into(),
                    bytes: b"core\r\n".to_vec()
                },
            ]
        );
    }

    #[test]
    fn oversized_skill_file_is_refused() {
        assert!(matches!(
            read_files(&[("gh/SKILL.md", &vec![b'x'; MAX_SKILL_FILE_BYTES + 1])]),
            Err(SkillReadError::SkillFileTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_resource_is_refused() {
        assert!(matches!(
            read_files(&[
                ("gh/SKILL.md", b"s"),
                ("gh/r", &vec![b'x'; MAX_SKILL_RESOURCE_BYTES + 1])
            ]),
            Err(SkillReadError::ResourceTooLarge { .. })
        ));
    }

    #[test]
    fn excess_resources_are_refused() {
        let paths = (0..=MAX_SKILL_RESOURCES)
            .map(|n| format!("gh/r{n}"))
            .collect::<Vec<_>>();
        let mut files = vec![("gh/SKILL.md", &b"s"[..])];
        files.extend(paths.iter().map(|path| (path.as_str(), &b""[..])));
        assert!(matches!(
            read_files(&files),
            Err(SkillReadError::TooManyResources { .. })
        ));
    }

    #[test]
    fn excess_resource_total_is_refused() {
        let resource = vec![b'x'; MAX_SKILL_RESOURCE_BYTES];
        let paths = (0..=MAX_SKILL_TOTAL_BYTES / MAX_SKILL_RESOURCE_BYTES)
            .map(|n| format!("gh/r{n}"))
            .collect::<Vec<_>>();
        let mut files = vec![("gh/SKILL.md", &b"s"[..])];
        files.extend(
            paths
                .iter()
                .map(|path| (path.as_str(), resource.as_slice())),
        );
        assert!(matches!(
            read_files(&files),
            Err(SkillReadError::ResourcesTooLarge { .. })
        ));
    }

    #[test]
    fn excess_directory_depth_is_refused() {
        let path = format!("gh/{}file", "dir/".repeat(MAX_SKILL_RESOURCE_DEPTH + 1));
        assert!(matches!(
            read_files(&[("gh/SKILL.md", b"s"), (&path, b"r")]),
            Err(SkillReadError::TooDeep { .. })
        ));
    }

    #[test]
    fn non_utf8_payload_is_refused() {
        assert!(matches!(
            read_files(&[("gh/SKILL.md", &[0xff])]),
            Err(SkillReadError::NotUtf8 { .. })
        ));
    }

    #[test]
    fn another_providers_skill_is_refused() {
        assert!(matches!(
            read_files(&[("github/SKILL.md", b"s")]),
            Err(SkillReadError::ProviderMismatch { .. })
        ));
        assert!(read_files(&[("gh-review/SKILL.md", b"s")]).is_ok());
    }

    #[test]
    fn unsafe_path_segments_are_refused() {
        assert!(matches!(
            read_files(&[("gh//file", b"s")]),
            Err(SkillReadError::EmptySegment { .. })
        ));
        assert!(matches!(
            read_files(&[("gh/./file", b"s")]),
            Err(SkillReadError::DotSegment { .. })
        ));
        assert!(matches!(
            read_files(&[("gh/../file", b"s")]),
            Err(SkillReadError::ParentSegment { .. })
        ));
        assert!(matches!(
            read_files(&[("gh/.hidden/file", b"s")]),
            Err(SkillReadError::HiddenSegment { .. })
        ));
    }

    #[test]
    fn duplicate_paths_across_nesting_levels_are_refused() {
        let mut module = Module::new();
        module.section(&section("gh/SKILL.md", b"second"));
        let mut root = Component::new();
        root.section(&section("gh/SKILL.md", b"first"));
        root.section(&ModuleSection(&module));
        assert!(matches!(
            read(&"gh".parse().unwrap(), &root.finish()),
            Err(SkillReadError::DuplicatePath { .. })
        ));
    }

    #[test]
    fn file_directory_collisions_are_refused_in_either_order() {
        for files in [
            vec![("gh/a", &b"a"[..]), ("gh/a/b", &b"b"[..])],
            vec![("gh/a/b", &b"b"[..]), ("gh/a", &b"a"[..])],
        ] {
            assert!(matches!(
                read_files(&files),
                Err(SkillReadError::PathIsDirectory { .. })
            ));
        }
    }

    #[test]
    fn missing_skill_file_is_refused() {
        assert!(matches!(
            read_files(&[("gh/resource", b"r")]),
            Err(SkillReadError::MissingSkillFile { .. })
        ));
    }
}
