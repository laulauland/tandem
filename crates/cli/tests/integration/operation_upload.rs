//! Operation metadata batching through the stock OpStore interface and real HTTP.
use crate::common;
use crate::common::ServerFixture;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OpStore as _, Operation, RootOperationData, View};
use jj_tandem_client::op_store::TandemOpStore;
use jj_tandem_jj::proto_convert;
use pollster::FutureExt as _;
use prost::Message as _;

fn store(fx: &ServerFixture, name: &str) -> TandemOpStore {
    TandemOpStore::init(
        &fx.dir(name),
        &fx.addr,
        fx.token(),
        RootOperationData {
            root_commit_id: CommitId::from_bytes(&[0; 20]),
        },
    )
    .unwrap()
}

fn view(value: u8) -> View {
    View::make_root(CommitId::from_bytes(&[value; 20]))
}

fn operation(store: &TandemOpStore, view: jj_lib::op_store::ViewId) -> Operation {
    let mut operation = Operation::make_root(view);
    operation.parents.push(store.root_operation_id().clone());
    operation
}

#[test]
fn pending_views_read_flush_and_upload_with_exact_semantic_ids() {
    let fx = ServerFixture::builder().log_to_file().start();
    let store = store(&fx, "metadata");
    let first = view(31);
    let first_id = store.write_view(&first).block_on().unwrap();
    assert_eq!(
        first_id.as_bytes(),
        &jj_lib::content_hash::blake2b_hash(&first)[..]
    );
    assert_eq!(store.read_view(&first_id).block_on().unwrap(), first);
    assert_eq!(fx.rpc_request_count("putView"), 0);
    let second = view(32);
    let second_id = store.write_view(&second).block_on().unwrap();
    assert_eq!(fx.rpc_request_count("putView"), 1);
    let bytes = common::api_get(
        &fx.addr,
        fx.token(),
        &format!("/api/views/{}", first_id.hex()),
    )
    .bytes()
    .unwrap();
    assert_eq!(
        &bytes[..],
        proto_convert::view_to_proto(&first).encode_to_vec()
    );
    let operation = operation(&store, second_id.clone());
    let id = store.write_operation(&operation).block_on().unwrap();
    assert_eq!(
        id.as_bytes(),
        &jj_lib::content_hash::blake2b_hash(&operation)[..]
    );
    assert_eq!(fx.rpc_request_count("putOperationWithView"), 1);
    assert_eq!(store.read_operation(&id).block_on().unwrap(), operation);
    assert_eq!(store.read_view(&second_id).block_on().unwrap(), second);
}

#[test]
fn failed_upload_retains_view_for_reads_and_retry() {
    let mut fx = ServerFixture::start();
    let store = store(&fx, "metadata");
    let contents = view(43);
    let id = store.write_view(&contents).block_on().unwrap();
    let operation = operation(&store, id.clone());
    fx.stop();
    assert!(store.write_operation(&operation).block_on().is_err());
    assert_eq!(store.read_view(&id).block_on().unwrap(), contents);
    fx.restart_with_env(&[]);
    let operation_id = store.write_operation(&operation).block_on().unwrap();
    assert_eq!(
        store.read_operation(&operation_id).block_on().unwrap(),
        operation
    );
    assert_eq!(store.read_view(&id).block_on().unwrap(), contents);
}

#[test]
fn concurrent_calls_keep_each_view_reachable() {
    let fx = ServerFixture::start();
    let store = store(&fx, "metadata");
    std::thread::scope(|scope| {
        let handles: Vec<_> = (50..58)
            .map(|value| {
                let store = &store;
                scope.spawn(move || {
                    let contents = view(value);
                    let id = store.write_view(&contents).block_on().unwrap();
                    let operation = operation(store, id.clone());
                    let operation_id = store.write_operation(&operation).block_on().unwrap();
                    assert_eq!(
                        store.read_operation(&operation_id).block_on().unwrap(),
                        operation
                    );
                    assert_eq!(store.read_view(&id).block_on().unwrap(), contents);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

#[test]
fn upload_authenticates_and_rejects_mismatched_view_before_staging() {
    let fx = ServerFixture::start();
    let store = store(&fx, "metadata");
    let contents = view(73);
    let id = jj_lib::op_store::ViewId::new(jj_lib::content_hash::blake2b_hash(&contents).to_vec());
    let wrong_id = jj_lib::op_store::ViewId::from_bytes(&[1; 64]);
    let operation = operation(&store, wrong_id);
    let body = jj_tandem_protocol::wire::encode_operation_upload(
        &proto_convert::view_to_proto(&contents).encode_to_vec(),
        &proto_convert::operation_to_proto(&operation).encode_to_vec(),
    );
    let url = common::api_url(&fx.addr, "/api/ops:upload");
    assert_eq!(
        common::http_client()
            .post(&url)
            .body(body.clone())
            .send()
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert!(!common::http_client()
        .post(&url)
        .bearer_auth(fx.token())
        .body(body)
        .send()
        .unwrap()
        .status()
        .is_success());
    assert_eq!(
        common::http_client()
            .get(common::api_url(
                &fx.addr,
                &format!("/api/views/{}", id.hex())
            ))
            .bearer_auth(fx.token())
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404
    );
}

#[test]
fn combined_upload_rejects_unexpected_operation_response_id() {
    let fx = ServerFixture::start();
    let store = store(&fx, "metadata");
    let contents = view(81);
    let id = store.write_view(&contents).block_on().unwrap();
    let operation = operation(&store, id);
    let client =
        jj_tandem_client::http_client::TandemClient::connect(&fx.addr, fx.token()).unwrap();
    let error = client
        .put_operation_with_view(
            &proto_convert::view_to_proto(&contents).encode_to_vec(),
            &proto_convert::operation_to_proto(&operation).encode_to_vec(),
            &[1; 64],
            operation.view_id.as_bytes(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("unexpected ID"));
    let error = client
        .put_operation_with_view(
            &proto_convert::view_to_proto(&contents).encode_to_vec(),
            &proto_convert::operation_to_proto(&operation).encode_to_vec(),
            &jj_lib::content_hash::blake2b_hash(&operation),
            &[2; 64],
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("view upload returned an unexpected ID"));

    store.write_operation(&operation).block_on().unwrap();
}

#[test]
fn oversized_view_uploads_without_entering_pending_buffer() {
    use jj_lib::ref_name::WorkspaceNameBuf;
    let fx = ServerFixture::builder().log_to_file().start();
    let store = store(&fx, "metadata");
    let mut contents = view(83);
    contents.wc_commit_ids.insert(
        WorkspaceNameBuf::from("x".repeat(1024 * 1024)),
        CommitId::from_bytes(&[83; 20]),
    );
    let id = store.write_view(&contents).block_on().unwrap();
    assert_eq!(fx.rpc_request_count("putView"), 1);
    assert_eq!(store.read_view(&id).block_on().unwrap(), contents);
}

#[test]
fn oversized_operation_metadata_is_rejected_before_the_view_is_stored() {
    let fx = ServerFixture::builder().log_to_file().start();
    let store = store(&fx, "metadata");
    let contents = view(91);
    let view_id = store.write_view(&contents).block_on().unwrap();
    let mut operation = operation(&store, view_id.clone());
    operation.metadata.description = "o".repeat(3 * 1024 * 1024);
    let error = store.write_operation(&operation).block_on().unwrap_err();
    assert!(error.to_string().contains("operation needs"), "{error:#}");
    assert_eq!(fx.rpc_request_count("putOperationWithView"), 1);
    assert_eq!(fx.rpc_request_count("putView"), 0);
    assert_eq!(fx.rpc_request_count("putOperation"), 0);
    assert_eq!(
        common::http_client()
            .get(common::api_url(
                &fx.addr,
                &format!("/api/views/{}", view_id.hex()),
            ))
            .bearer_auth(fx.token())
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404,
        "rejected paired metadata must not leave its view stored",
    );
}
