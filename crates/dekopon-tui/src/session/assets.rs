use std::{collections::BTreeMap, os::fd::OwnedFd};

use dekopon_agent::attachment::{
    ChatAssetRefusal, ChatAssetSource, GeneratedAssetStore, GeneratedImage, ResolveFuture,
    ResolvedInput,
};
use dekopon_broker_protocol::{AssetEncoding, AssetRow, NewAsset};
use dekopon_core::base64::{Engine as _, STANDARD};
use dekopon_model::asset::{BlobError, DiskBlob};
use parking_lot::Mutex;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ITEMS: usize = 16;
const MAX_ENCODED: usize = 1024 * 1024;
const MAX_DECODED: usize = 512 * 1024;

#[derive(Default)]
pub struct ConsoleAssets(Mutex<Inventory>);

#[derive(Default)]
struct Inventory {
    next_id: u64,
    bytes: usize,
    items: BTreeMap<u64, (AssetRow, DiskBlob)>,
}

impl ConsoleAssets {
    fn insert(
        &self,
        blob: DiskBlob,
        content_type: String,
        encoding: AssetEncoding,
        origin: String,
    ) -> Result<u64, BlobError> {
        let len = blob.len();
        let mut inventory = self.0.lock();
        if inventory.items.len() >= MAX_ITEMS
            || len > MAX_BYTES
            || inventory.bytes > MAX_BYTES - len
        {
            return Err(BlobError::Capacity);
        }
        let id = inventory
            .next_id
            .checked_add(1)
            .ok_or(BlobError::Capacity)?;
        inventory.next_id = id;
        inventory.bytes += len;
        inventory.items.insert(
            id,
            (
                AssetRow {
                    id,
                    content_type,
                    encoding,
                    bytes: Some(len as u64),
                    origin,
                    sent: false,
                },
                blob,
            ),
        );
        Ok(id)
    }

    /// Decode only an operator-entered, bounded standard-base64 literal; never store encoded text.
    pub fn attach_base64(&self, mime: &str, encoded: &str) -> Result<u64, String> {
        let valid_mime = mime.len() <= 127
            && !mime.is_empty()
            && mime.is_ascii()
            && mime.split_once('/').is_some_and(|(kind, subtype)| {
                !kind.is_empty()
                    && !subtype.is_empty()
                    && kind.bytes().chain(subtype.bytes()).all(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(
                                b,
                                b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                            )
                    })
            });
        if !valid_mime {
            return Err("invalid MIME type".into());
        }
        if encoded.len() > MAX_ENCODED {
            return Err("encoded asset exceeds 1 MiB".into());
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| format!("invalid standard base64: {error}"))?;
        if bytes.len() > MAX_DECODED {
            return Err("decoded asset exceeds 512 KiB".into());
        }
        let blob = DiskBlob::from_bytes(&bytes).map_err(|error| error.to_string())?;
        self.insert(
            blob,
            mime.to_owned(),
            AssetEncoding::Identity,
            "console".into(),
        )
        .map_err(|error| error.to_string())
    }
}

impl ChatAssetSource for ConsoleAssets {
    fn resolve_input(&self, id: u64) -> ResolveFuture<'_> {
        Box::pin(async move {
            let inventory = self.0.lock();
            let (row, blob) = inventory
                .items
                .get(&id)
                .ok_or(ChatAssetRefusal::UnknownAsset)?;
            Ok(ResolvedInput {
                row: row.clone(),
                blob: blob.clone(),
            })
        })
    }
}

impl GeneratedAssetStore for ConsoleAssets {
    fn register(
        &self,
        descriptor: OwnedFd,
        metadata: &NewAsset,
        _capability: &str,
        _invocation: &str,
    ) -> Result<u64, BlobError> {
        if metadata.bytes > MAX_BYTES as u64 {
            return Err(BlobError::TooLarge);
        }
        let blob = DiskBlob::from_descriptor(descriptor, metadata.bytes as usize)?;
        self.insert(
            blob,
            metadata.content_type.clone(),
            metadata.encoding,
            "provider".into(),
        )
    }

    fn remove(&self, id: u64) -> Result<(), BlobError> {
        let mut inventory = self.0.lock();
        let (_, blob) = inventory.items.get(&id).ok_or(BlobError::Unknown)?;
        if blob.is_pinned() {
            return Err(BlobError::Capacity);
        }
        let (_, blob) = inventory.items.remove(&id).ok_or(BlobError::Unknown)?;
        inventory.bytes -= blob.len();
        Ok(())
    }

    fn send(&self, _id: u64) -> Result<Option<GeneratedImage>, BlobError> {
        Err(BlobError::Disabled)
    }

    fn delivery_failed(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_agent::attachment::{ChatAssetInputs, PreparedRun};
    use dekopon_core::AssetNumber;
    use std::io::Write as _;

    #[tokio::test]
    async fn returned_local_removed_and_reentered() {
        let store = std::sync::Arc::new(ConsoleAssets::default());
        let local = store.attach_base64("image/heic", "AAEC/w==").unwrap();
        let ResolvedInput { row, blob } = store.resolve_input(local).await.unwrap();
        assert_eq!(row.content_type, "image/heic");
        assert_eq!(blob.read().unwrap(), [0, 1, 2, 255]);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"returned").unwrap();
        let returned = store
            .register(
                file.into(),
                &NewAsset {
                    descriptor: 0,
                    content_type: "image/png".into(),
                    encoding: AssetEncoding::Identity,
                    bytes: 8,
                    sha256: String::new(),
                },
                "x",
                "y",
            )
            .unwrap();
        assert_eq!(
            store
                .resolve_input(returned)
                .await
                .unwrap()
                .blob
                .read()
                .unwrap(),
            b"returned"
        );
        let inputs = ChatAssetInputs::new(std::sync::Arc::<ConsoleAssets>::clone(&store));
        let PreparedRun {
            assets: prepared,
            pins,
            ..
        } = inputs
            .prepare_run(
                &[dekopon_shell::Arg::Asset(
                    AssetNumber::parse("chat-asset:2").unwrap(),
                )],
                0,
            )
            .await
            .unwrap();
        assert_eq!(prepared.descriptors.len(), 1);
        assert_eq!(pins[0].read().unwrap(), b"returned");
        use std::os::unix::fs::FileExt as _;
        let mut descriptor_bytes = vec![0; 8];
        std::fs::File::from(prepared.descriptors[0].try_clone().unwrap())
            .read_exact_at(&mut descriptor_bytes, 0)
            .unwrap();
        assert_eq!(descriptor_bytes, b"returned");
        drop(pins);
        drop(prepared);
        assert_eq!(store.0.lock().items.len(), 2);
        assert_eq!(
            store.resolve_input(1).await.unwrap().row.encoding,
            AssetEncoding::Identity
        );
        assert_eq!(
            store.resolve_input(1).await.unwrap().row.content_type,
            "image/heic"
        );
        assert_eq!(
            store.resolve_input(2).await.unwrap().row.content_type,
            "image/png"
        );
        drop(blob);
        store.remove(local).unwrap();
        assert!(matches!(
            inputs
                .prepare_run(
                    &[dekopon_shell::Arg::Asset(
                        AssetNumber::parse("chat-asset:1").unwrap()
                    )],
                    0
                )
                .await,
            Err(ChatAssetRefusal::UnknownAsset)
        ));
        assert!(matches!(
            store.resolve_input(local).await,
            Err(ChatAssetRefusal::UnknownAsset)
        ));
        let next_entry = ConsoleAssets::default();
        assert!(matches!(
            next_entry.resolve_input(returned).await,
            Err(ChatAssetRefusal::UnknownAsset)
        ));
        assert_eq!(
            next_entry.attach_base64("image/heic", "AAEC/w==").unwrap(),
            1
        );
        let other_agent = ConsoleAssets::default();
        assert_eq!(
            other_agent.attach_base64("text/plain", "b3RoZXI=").unwrap(),
            1
        );
        assert_eq!(
            other_agent
                .resolve_input(1)
                .await
                .unwrap()
                .blob
                .read()
                .unwrap(),
            b"other"
        );
        assert!(matches!(
            other_agent.resolve_input(returned).await,
            Err(ChatAssetRefusal::UnknownAsset)
        ));
    }

    #[test]
    fn base64_bounds_and_malformed() {
        let store = ConsoleAssets::default();
        for (mime, data) in [
            ("image/heic", "%%%"),
            ("not mime", "AA=="),
            ("image/heic", "AA=!"),
        ] {
            assert!(store.attach_base64(mime, data).is_err());
        }
        assert!(
            store
                .attach_base64("image/heic", &"A".repeat(MAX_ENCODED + 1))
                .is_err()
        );
        assert!(
            store
                .attach_base64("image/heic", &"A".repeat(699_052))
                .is_err()
        );
        assert!(store.0.lock().items.is_empty());
    }
}
