//! 可选的媒体类型降级，服务不支持 OCI 声明能力的客户端。
//!
//! 部分上游（典型是 ghcr.io）只提供 OCI 媒体类型，且不做格式降级：
//! 客户端 `Accept` 头里没有 OCI 类型时直接回 404
//! `OCI index found, but accept header does not support OCI indexes`。
//! 这符合 OCI Distribution Spec，但对只能发 Docker 类型的老客户端不友好。
//!
//! 开启 `DONKEY_MEDIA_TYPE_FALLBACK=true` 后由代理接管这次协商：
//!
//! 1. 向上游请求时把 OCI 类型补进 `Accept`，让上游正常返回；
//! 2. 返回客户端前，把响应里的 OCI 媒体类型改写成 Docker 等价类型。
//!
//! 只改 `mediaType` 字符串和 `Content-Type` 头，layer / config 的字节与
//! digest 一律不动——这正是 registry 侧降级会做的事，索引结构完全一致。

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";

/// manifest 响应体积上限。index / manifest 通常几 KB，留足余量。
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

/// 客户端 `Accept` 里是否显式声明了 OCI 支持。
///
/// 只认 `oci.image` 字样：缺头、`*/*`、仅 Docker 类型都算「不支持」，
/// 与 ghcr.io 的判定口径保持一致。
pub fn client_supports_oci(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("oci.image"))
}

/// 把 OCI 类型补进 `Accept`，让只提供 OCI 的上游肯返回内容。
///
/// 客户端原有偏好顺序保持不变，补在末尾。
pub fn upgrade_accept(headers: &mut HeaderMap) {
    let existing = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_owned();
    let upgraded = if existing.is_empty() {
        format!("{OCI_INDEX}, {OCI_MANIFEST}, {DOCKER_LIST}, {DOCKER_MANIFEST}")
    } else {
        format!("{existing}, {OCI_INDEX}, {OCI_MANIFEST}")
    };
    if let Ok(value) = HeaderValue::from_str(&upgraded) {
        headers.insert(header::ACCEPT, value);
    }
}

/// OCI 媒体类型对应的 Docker 等价类型。
pub fn docker_equivalent(media_type: &str) -> Option<&'static str> {
    match media_type {
        OCI_INDEX => Some(DOCKER_LIST),
        OCI_MANIFEST => Some(DOCKER_MANIFEST),
        OCI_CONFIG => Some(DOCKER_CONFIG),
        _ => None,
    }
}

/// 递归改写 JSON 里的所有 `mediaType` 字段。
///
/// index 顶层、`manifests[]`、manifest 的 `config`/`layers[]` 都覆盖到。
fn rewrite_media_types(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(map) => {
            if let Some(Value::String(current)) = map.get("mediaType")
                && let Some(replacement) = docker_equivalent(current)
            {
                map.insert(
                    "mediaType".to_owned(),
                    Value::String(replacement.to_owned()),
                );
                changed = true;
            }
            for (_, nested) in map.iter_mut() {
                changed |= rewrite_media_types(nested);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                changed |= rewrite_media_types(item);
            }
        }
        _ => {}
    }
    changed
}

/// 把上游的 OCI manifest 响应改写成 Docker 类型。
///
/// 非 OCI 响应、错误状态、非 JSON 内容一律原样返回。
pub async fn downgrade_response(response: Response) -> Response {
    let status = response.status();
    if !status.is_success() {
        return response;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        });
    let Some(target) = content_type.as_deref().and_then(docker_equivalent) else {
        return response;
    };

    let (mut parts, body) = response.into_parts();

    // HEAD 请求没有 body：只改 Content-Type，不解析 JSON，也不动 Content-Length。
    if parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        == Some("0")
    {
        if let Ok(value) = HeaderValue::from_str(target) {
            parts.headers.insert(header::CONTENT_TYPE, value);
        }
        return Response::from_parts(parts, body);
    }

    let Ok(bytes) = to_bytes(body, MAX_MANIFEST_BYTES).await else {
        // 读取失败就直接给客户端一个明确的错误，避免半截 body。
        return fallback_error();
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return fallback_error();
    };
    if !rewrite_media_types(&mut value) {
        return fallback_error();
    }
    let Ok(encoded) = serde_json::to_vec(&value) else {
        return fallback_error();
    };

    if let Ok(value) = HeaderValue::from_str(target) {
        parts.headers.insert(header::CONTENT_TYPE, value);
    }
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(encoded.len()));
    // 内容变了，缓存的 ETag 不再对应，交给客户端重新协商。
    parts.headers.remove(header::ETAG);

    Response::from_parts(parts, Body::from(encoded))
}

/// 改写失败时的兜底响应：明确告知客户端降级没有完成，
/// 而不是返回半截 body 让 Docker 报更难懂的错。
fn fallback_error() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        r#"{"errors":[{"code":"UNKNOWN","message":"media type fallback failed"}]}"#,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn detects_client_capability() {
        let mut headers = HeaderMap::new();
        assert!(!client_supports_oci(&headers));

        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!client_supports_oci(&headers));

        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.docker.distribution.manifest.v2+json"),
        );
        assert!(!client_supports_oci(&headers));

        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(
                "application/vnd.docker.distribution.manifest.list.v2+json, \
                 application/vnd.oci.image.index.v1+json",
            ),
        );
        assert!(client_supports_oci(&headers));
    }

    #[test]
    fn upgrades_accept_preserving_order() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.docker.distribution.manifest.v2+json"),
        );
        upgrade_accept(&mut headers);
        let value = headers.get(header::ACCEPT).unwrap().to_str().unwrap();
        assert!(value.starts_with("application/vnd.docker.distribution.manifest.v2+json"));
        assert!(value.contains(OCI_INDEX));
    }

    #[test]
    fn rewrites_index_and_manifest_fields() {
        let mut value = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [
                { "mediaType": OCI_MANIFEST, "digest": "sha256:aa" }
            ]
        });
        assert!(rewrite_media_types(&mut value));
        assert_eq!(value["mediaType"], DOCKER_LIST);
        assert_eq!(value["manifests"][0]["mediaType"], DOCKER_MANIFEST);
    }

    #[test]
    fn rewrites_config_and_layers() {
        let mut value = serde_json::json!({
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": OCI_MANIFEST },
            "layers": [ { "mediaType": OCI_MANIFEST } ]
        });
        assert!(rewrite_media_types(&mut value));
        assert_eq!(value["mediaType"], DOCKER_MANIFEST);
        assert_eq!(value["config"]["mediaType"], DOCKER_MANIFEST);
        assert_eq!(value["layers"][0]["mediaType"], DOCKER_MANIFEST);
    }

    #[test]
    fn rewrites_config_media_type() {
        let mut value = serde_json::json!({
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": OCI_CONFIG }
        });
        assert!(rewrite_media_types(&mut value));
        assert_eq!(value["config"]["mediaType"], DOCKER_CONFIG);
    }

    #[test]
    fn leaves_docker_payload_untouched() {
        let mut value = serde_json::json!({
            "mediaType": DOCKER_LIST,
            "manifests": [ { "mediaType": DOCKER_MANIFEST } ]
        });
        assert!(!rewrite_media_types(&mut value));
    }

    #[tokio::test]
    async fn downgrades_oci_index_response() {
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [ { "mediaType": OCI_MANIFEST, "digest": "sha256:aa" } ]
        });
        let mut response = Response::new(Body::from(body.to_string()));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(OCI_INDEX),
        );
        let downgraded = downgrade_response(response).await;
        assert_eq!(
            downgraded.headers().get(header::CONTENT_TYPE).unwrap(),
            DOCKER_LIST
        );
        let bytes = to_bytes(downgraded.into_body(), MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["mediaType"], DOCKER_LIST);
    }

    #[tokio::test]
    async fn ignores_non_oci_responses() {
        let mut response = Response::new(Body::from("{\"schemaVersion\":2}"));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let untouched = downgrade_response(response).await;
        assert_eq!(
            untouched.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
    }

    #[tokio::test]
    async fn passes_error_status_through() {
        let response = (StatusCode::NOT_FOUND, "").into_response();
        let untouched = downgrade_response(response).await;
        assert_eq!(untouched.status(), StatusCode::NOT_FOUND);
    }
}
