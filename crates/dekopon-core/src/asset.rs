pub const MAX_DECODED_ASSET_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DECODED_INVOCATION_BYTES: usize = 40 * 1024 * 1024;
pub const MAX_ASSET_CHUNK_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Image,
    TextDocument,
    BinaryDocument,
    Other,
}

#[cfg(test)]
mod number_encoding_tests {
    use crate::AssetNumber;

    #[test]
    fn asset_numbers_encode_as_numbers_and_refuse_zero() {
        let number = AssetNumber::try_from(42).unwrap();
        assert_eq!(serde_json::to_string(&number).unwrap(), "42");
        assert_eq!(serde_json::from_str::<AssetNumber>("42").unwrap(), number);
        assert!(serde_json::from_str::<AssetNumber>("0").is_err());
    }
}
