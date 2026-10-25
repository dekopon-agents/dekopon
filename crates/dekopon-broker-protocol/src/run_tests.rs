use super::*;
use serde_json::json;

fn golden() -> Value {
    json!({"apiVersion":"dekopon.dev/broker/v1alpha2","request":{"operation":"run","traceParent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01","invocation":"inv-1","word":"image","argv":[{"text":"edit"},{"asset":0},{"text":"--prompt"},{"text":"make it blue"},{"asset":1},{"asset":0}],"assets":[{"id":7,"contentType":"image/png","encoding":"identity","bytes":21944,"origin":"chat","sent":false},{"id":3,"contentType":"image/png","encoding":"base64","bytes":4096,"origin":"provider:image.generate","sent":true}],"sendsRemaining":4,"streams":{"stdin":false}}})
}

#[tokio::test]
async fn a_run_frame_round_trips_its_argv_rows_and_descriptors() {
    let value = golden();
    let request: RequestEnvelope = serde_json::from_value(value.clone()).unwrap();
    request.request.validate().unwrap();
    assert_eq!(serde_json::to_value(&request).unwrap(), value);
    let (left, right) = UnixStream::pair().unwrap();
    let mut sender = DescriptorStream::new(left);
    let mut receiver = DescriptorStream::new(right);
    let files = [tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap()];
    let (stdout, _) = std::os::unix::net::UnixStream::pair().unwrap();
    let descriptors = [files[0].as_fd(), files[1].as_fd(), stdout.as_fd()];
    sender
        .write_frame(&request, &descriptors, FrameLimits::default())
        .await
        .unwrap();
    let (received, mut descriptors) = receiver
        .read_frame::<RequestEnvelope>(FrameLimits::default())
        .await
        .unwrap();
    assert_eq!(received, request);
    assert_eq!(descriptors.len(), 3);
    let streams =
        Streams::split_from(&mut descriptors, StreamDescriptors { stdin: false }).unwrap();
    assert!(streams.stdin.is_none());
    for (file, descriptor) in files.iter().zip(descriptors) {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            file.metadata().unwrap().ino(),
            std::fs::File::from(descriptor).metadata().unwrap().ino()
        );
    }
}

#[test]
fn a_frame_never_names_a_source() {
    let request: RequestEnvelope = serde_json::from_value(golden()).unwrap();
    let value = serde_json::to_value(request).unwrap();
    for row in value["request"]["assets"].as_array().unwrap() {
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["bytes", "contentType", "encoding", "id", "origin", "sent"]
                .into_iter()
                .collect()
        );
    }
}

#[test]
fn an_unreferenced_row_is_refused() {
    let mut value = golden();
    value["request"]["argv"] = json!([{"asset":0}]);
    let request: RequestEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(
        request.request.validate(),
        Err(ProtocolError::UnreferencedAsset)
    ));
}

#[test]
fn a_duplicate_row_is_refused() {
    let mut value = golden();
    value["request"]["assets"][1]["id"] = json!(7);
    let request: RequestEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(
        request.request.validate(),
        Err(ProtocolError::DuplicateAssetRow)
    ));
}
