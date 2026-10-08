use dekopon_broker_host::asset::{AssetInputs, references};
use dekopon_broker_protocol::{AssetEncoding, AssetRow};
use dekopon_capability::AssetConstraints;
use dekopon_http_host::asset::{AssetDirectory, AssetFile};
use serde_json::Value;

use crate::typed::{HarnessError, runtime};

pub(crate) struct InputAsset {
    pub id: u64,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct Fixtures {
    pub directory: AssetDirectory,
    files: Vec<(AssetRow, AssetFile)>,
    _root: tempfile::TempDir,
}

impl Fixtures {
    pub fn new(
        grant: Option<&AssetConstraints>,
        inputs: Vec<InputAsset>,
    ) -> Result<Option<Self>, HarnessError> {
        if grant.is_none() && inputs.is_empty() {
            return Ok(None);
        }
        let root = tempfile::tempdir()?;
        let directory = AssetDirectory::new(
            root.path().to_owned(),
            dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES as u64,
        );
        let files = runtime().block_on(async {
            let mut files = Vec::new();
            for input in inputs {
                let mut spool = directory.allocate().await?;
                for chunk in input
                    .bytes
                    .chunks(dekopon_core::asset::MAX_ASSET_CHUNK_BYTES)
                {
                    spool = spool.write(chunk.to_vec()).await?;
                }
                files.push((
                    AssetRow {
                        id: input.id,
                        content_type: input.content_type,
                        encoding: AssetEncoding::Identity,
                        bytes: Some(input.bytes.len() as u64),
                        origin: "testkit".to_owned(),
                        sent: false,
                    },
                    spool.finish().await?,
                ));
            }
            Ok::<_, HarnessError>(files)
        })?;
        Ok(Some(Self {
            directory,
            files,
            _root: root,
        }))
    }

    pub fn inputs(&self, input: &Value) -> Result<AssetInputs, HarnessError> {
        let descriptors = references(input)
            .into_iter()
            .filter_map(|id| self.files.iter().find(|(row, _)| row.id == id))
            .map(|(_, file)| file.file().try_clone().map(Into::into))
            .collect::<Result<_, _>>()?;
        Ok(AssetInputs {
            rows: self.files.iter().map(|(row, _)| row.clone()).collect(),
            descriptors,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{FileExt as _, MetadataExt as _};

    #[test]
    fn fixtures_pass_only_referenced_read_only_files_and_remove_their_directory() {
        let fixtures = Fixtures::new(
            None,
            vec![InputAsset {
                id: 1,
                content_type: "image/png".to_owned(),
                bytes: b"fixture bytes".to_vec(),
            }],
        )
        .unwrap()
        .unwrap();
        let path = fixtures._root.path().to_owned();
        let unreferenced = fixtures.inputs(&json!({})).unwrap();
        assert_eq!(unreferenced.rows.len(), 1);
        assert!(unreferenced.descriptors.is_empty());
        let mut inputs = fixtures.inputs(&json!({"image":"chat-asset:1"})).unwrap();
        let file = std::fs::File::from(inputs.descriptors.pop().unwrap());
        assert_eq!(file.metadata().unwrap().nlink(), 0);
        assert!(file.write_at(b"changed", 0).is_err());
        drop(fixtures);
        assert!(!path.exists());
        let mut bytes = [0; 13];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"fixture bytes");
    }
}
