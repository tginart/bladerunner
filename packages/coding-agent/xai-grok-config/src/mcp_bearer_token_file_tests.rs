use super::*;
#[test]
fn path_metadata_and_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let configured = path.to_str().unwrap();
    assert_eq!(
        BearerTokenPath::from_meta(Some(&bearer_token_file_meta(configured)))
            .unwrap()
            .unwrap()
            .as_ref(),
        path
    );
    assert_eq!(BearerTokenPath::from_meta(None).unwrap(), None);
    assert_eq!(
        BearerTokenPath::parse("~/token", Some(dir.path()))
            .unwrap()
            .as_ref(),
        path
    );
    assert!(matches!(
        BearerTokenPath::parse("~/token", None),
        Err(BearerTokenPathError::NoHomeDir(_))
    ));
    for value in ["token", "~other/token", ""] {
        assert!(BearerTokenPath::from_meta(Some(&bearer_token_file_meta(value))).is_err());
    }
    let meta = Meta::from_iter([(META_KEY.to_owned(), serde_json::json!(7))]);
    assert_eq!(
        BearerTokenPath::from_meta(Some(&meta)),
        Err(BearerTokenPathError::NotAString)
    );
    assert_eq!(
        configured_bearer_token_file(Some(&bearer_token_file_meta("~/token"))).as_deref(),
        Some("~/token")
    );
}
