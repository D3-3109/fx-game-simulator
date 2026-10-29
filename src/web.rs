use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, OnceCell};

struct Asset {
    data: Bytes,
    etag: String,
}

struct StaticFiles {
    dir: PathBuf,
    assets: Mutex<HashMap<String, Arc<OnceCell<Asset>>>>,
}

pub fn static_router(dir: PathBuf) -> Router {
    let files = Arc::new(StaticFiles {
        dir,
        assets: Mutex::new(HashMap::new()),
    });
    Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
        static_file(files.clone(), uri, headers)
    }))
}

pub fn static_target(path: &str) -> Option<(&str, &str, bool)> {
    Some(match path {
        "/" => ("index.html", "text/html; charset=utf-8", false),
        "/admin" => ("admin.html", "text/html; charset=utf-8", false),
        "/app.js" => ("app.js", "text/javascript", false),
        "/admin.js" => ("admin.js", "text/javascript", false),
        "/app.css" => ("app.css", "text/css; charset=utf-8", false),
        "/admin.css" => ("admin.css", "text/css; charset=utf-8", false),
        "/assets/lightweight-charts-5.2.1.js" => (
            "assets/lightweight-charts-5.2.1.js",
            "text/javascript",
            true,
        ),
        "/assets/noto-sans-latin.woff2" => ("assets/noto-sans-latin.woff2", "font/woff2", true),
        "/assets/Lightweight-Charts-LICENSE.txt" => (
            "assets/Lightweight-Charts-LICENSE.txt",
            "text/plain; charset=utf-8",
            false,
        ),
        "/assets/Lightweight-Charts-NOTICE.txt" => (
            "assets/Lightweight-Charts-NOTICE.txt",
            "text/plain; charset=utf-8",
            false,
        ),
        "/assets/OFL-NotoSans.txt" => (
            "assets/OFL-NotoSans.txt",
            "text/plain; charset=utf-8",
            false,
        ),
        _ => return None,
    })
}

pub async fn validate_static(dir: &Path) -> std::io::Result<PathBuf> {
    let root = tokio::fs::canonicalize(dir).await?;
    for url in [
        "/",
        "/admin",
        "/app.js",
        "/admin.js",
        "/app.css",
        "/admin.css",
        "/assets/lightweight-charts-5.2.1.js",
        "/assets/noto-sans-latin.woff2",
    ] {
        let target = tokio::fs::canonicalize(root.join(static_target(url).unwrap().0)).await?;
        if !target.starts_with(&root) || !target.is_file() {
            return Err(std::io::Error::other("invalid static resource path"));
        }
    }
    Ok(root)
}

async fn static_file(files: Arc<StaticFiles>, uri: Uri, headers: HeaderMap) -> Response {
    let missing = || {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"ok":false})),
        )
            .into_response()
    };
    let Some((file, mime, immutable)) = static_target(uri.path()) else {
        return missing();
    };
    let cell = {
        let mut assets = files.assets.lock().await;
        assets
            .entry(file.into())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    };
    // A running process serves one asset version; deploy assets together and restart.
    let loaded = cell
        .get_or_try_init(|| async {
            let path = tokio::fs::canonicalize(files.dir.join(file)).await?;
            if !path.starts_with(&files.dir) {
                return Err(std::io::Error::other("invalid static resource path"));
            }
            let data = tokio::fs::read(path).await?;
            let etag = format!("\"{}\"", hex::encode(Sha256::digest(&data)));
            Ok(Asset {
                data: Bytes::from(data),
                etag,
            })
        })
        .await;
    let Ok(asset) = loaded else {
        return missing();
    };
    let unchanged = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == asset.etag);
    let response = Response::builder()
        .status(if unchanged {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::ETAG, &asset.etag)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CACHE_CONTROL,
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        );
    if unchanged {
        response.body(Body::empty()).unwrap()
    } else {
        response
            .header(header::CONTENT_LENGTH, asset.data.len())
            .body(Body::from(asset.data.clone()))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;
    #[tokio::test]
    async fn cached_asset_survives_source_removal_until_restart() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("app.js"), b"const version=1;")
            .await
            .unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let app = static_router(root.clone());
        let request = || {
            Request::builder()
                .uri("/app.js")
                .body(Body::empty())
                .unwrap()
        };
        let first = app.clone().oneshot(request()).await.unwrap();
        let expected = axum::body::to_bytes(first.into_body(), 1024).await.unwrap();
        tokio::fs::remove_file(dir.path().join("app.js"))
            .await
            .unwrap();
        let cached = app.oneshot(request()).await.unwrap();
        assert_eq!(cached.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(cached.into_body(), 1024)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            static_router(root)
                .oneshot(request())
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn static_assets_headers_and_traversal() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        let app = static_router(validate_static(&dir).await.unwrap());
        for path in [
            "/",
            "/admin",
            "/app.js",
            "/app.css",
            "/admin.js",
            "/admin.css",
            "/assets/lightweight-charts-5.2.1.js",
            "/assets/noto-sans-latin.woff2",
        ] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let (_, mime, immutable) = static_target(path).unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            assert_eq!(r.headers()[header::CONTENT_TYPE], mime);
            assert_eq!(
                r.headers()[header::CACHE_CONTROL],
                if immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                }
            );
        }
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
        for value in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"other\", {etag}"),
            "*".into(),
        ] {
            let cached = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/app.js")
                        .header(header::IF_NONE_MATCH, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(cached.headers()[header::ETAG], etag);
            assert!(
                axum::body::to_bytes(cached.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let changed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app.js")
                    .header(header::IF_NONE_MATCH, "\"different\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::OK);
        for path in [
            "/.pi/admin-key.json",
            "/fx_sim.db",
            "/assets/../.pi/admin-key.json",
            "/assets/%2e%2e/fx_sim.db",
            "/%252e%252e/fx_sim.db",
            "/assets/",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}
