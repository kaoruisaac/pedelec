use pedelec_lib::pedelec_ipc::{CoreIpcRequest, CoreIpcResponse, RuntimeFile};
use serde_json::json;

#[test]
fn public_ipc_wire_types_serialize_with_the_expected_shape() {
    let request = CoreIpcRequest {
        request_id: "request-1".into(),
        r#type: "listProviders".into(),
        payload: Some(json!({})),
    };
    let response = CoreIpcResponse {
        request_id: "request-1".into(),
        ok: true,
        result: Some(json!({"providers": []})),
        error: None,
    };
    let runtime = RuntimeFile {
        protocol: "pedelec-core-ipc-v1".into(),
        host: "127.0.0.1".into(),
        port: 4321,
        endpoint: "127.0.0.1:4321".into(),
        pid: 1,
    };

    assert_eq!(
        serde_json::to_value(request).unwrap()["requestId"],
        "request-1"
    );
    assert_eq!(
        serde_json::to_value(response).unwrap()["requestId"],
        "request-1"
    );
    assert_eq!(
        serde_json::to_value(runtime).unwrap()["protocol"],
        "pedelec-core-ipc-v1"
    );
}
