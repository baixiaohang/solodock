#![cfg(feature = "docker-e2e")]

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use solodock::docker::{
    client::BollardReadClient,
    image_cleanup::{ExactImageId, ImageCleanup, RemoveImageResult},
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Daemon {
    requests: Vec<(String, String)>,
    conflict: bool,
    absent: bool,
    incomplete: bool,
}
async fn endpoint(State(state): State<Arc<Mutex<Daemon>>>, request: Request) -> Response {
    let mut daemon = state.lock().unwrap();
    let path = request.uri().path();
    daemon
        .requests
        .push((request.method().to_string(), request.uri().to_string()));
    if path.ends_with("/version") {
        return Json(json!({"ApiVersion":"1.52","MinAPIVersion":"1.41","Os":"linux"}))
            .into_response();
    }
    if path.ends_with("/containers/json") {
        return Json(json!([{"Id":"a".repeat(64)},{"Id":"b".repeat(64)}])).into_response();
    }
    if path.contains("/containers/") {
        if daemon.incomplete {
            return StatusCode::NOT_FOUND.into_response();
        }
        let id = if path.contains(&"a".repeat(64)) {
            "a".repeat(64)
        } else {
            "b".repeat(64)
        };
        return Json(json!({"Id":id,"Name":"/unmanaged","Image":format!("sha256:{}","c".repeat(64)),"Config":{"Image":"operator/image","Labels":{}},"State":{"Status":"exited"}})).into_response();
    }
    if request.method() == "DELETE" {
        if daemon.conflict {
            return (StatusCode::CONFLICT, Json(json!({"message":"in use"}))).into_response();
        }
        daemon.absent = true;
        return Json(json!([])).into_response();
    }
    if daemon.absent {
        return (StatusCode::NOT_FOUND, Json(json!({"message":"absent"}))).into_response();
    }
    Json(json!({"Id":format!("sha256:{}","c".repeat(64)),"RepoDigests":[format!("example/image@sha256:{}","d".repeat(64))],"RepoTags":[],"Size":1024,"Os":"linux","Architecture":"amd64"})).into_response()
}

#[tokio::test]
async fn image_cleanup_adapter_is_all_container_exact_nonforce_noprune_and_fail_closed() {
    let state = Arc::new(Mutex::new(Daemon::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(endpoint).with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = BollardReadClient::for_test_http(format!("http://{address}")).image_cleanup();
    for invalid in ["alpine:latest", "sha256:1234", "../images", "*", "prune"] {
        assert!(ExactImageId::parse(invalid).is_err());
    }
    let containers = client.all_containers().await.unwrap();
    assert_eq!(containers.len(), 2);
    assert!(containers.iter().all(|c| c.labels.is_empty()));
    state.lock().unwrap().incomplete = true;
    assert!(client.all_containers().await.is_err());
    state.lock().unwrap().incomplete = false;
    let id = ExactImageId::parse(&format!("sha256:{}", "c".repeat(64))).unwrap();
    assert_eq!(
        client
            .inspect(&id)
            .await
            .unwrap()
            .unwrap()
            .reported_size_bytes,
        1024
    );
    state.lock().unwrap().conflict = true;
    assert_eq!(
        client.remove(&id).await.unwrap(),
        RemoveImageResult::Retained
    );
    assert!(client.inspect(&id).await.unwrap().is_some());
    state.lock().unwrap().conflict = false;
    assert_eq!(
        client.remove(&id).await.unwrap(),
        RemoveImageResult::Accepted
    );
    assert!(client.inspect(&id).await.unwrap().is_none());
    for (method, uri) in &state.lock().unwrap().requests {
        let parsed = url::Url::parse(&format!("http://fixture{uri}")).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        if parsed.path().ends_with("/containers/json") {
            assert_eq!(query.get("all").map(|v| v.as_ref()), Some("true"));
            assert!(query.get("filters").is_none_or(|v| v == "{}"));
        }
        if method == "DELETE" {
            assert!(parsed.path().ends_with(&format!("/images/{}", id.as_str())));
            assert_eq!(query.get("force").map(|v| v.as_ref()), Some("false"));
            assert_eq!(query.get("noprune").map(|v| v.as_ref()), Some("true"));
        }
        assert!(!uri.contains("/prune"));
    }
    task.abort();
}
