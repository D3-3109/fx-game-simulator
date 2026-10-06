use axum::{
    Router,
    body::Body,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn static_router(dir: PathBuf) -> Router {
    let dir = Arc::new(dir);
    Router::new().fallback(get(move |uri: Uri| static_file(dir.clone(), uri)))
}

pub fn static_target(path: &str) -> Option<(&str, &str, bool)> {
    Some(match path {
        "/" => ("index.html", "text/html; charset=utf-8", false),
        "/admin" => ("admin.html", "text/html; charset=utf-8", false),
        "/app.js" => ("app.js", "text/javascript", false),
        "/admin.js" => ("admin.js", "text/javascript", false),
        "/app.css" => ("app.css", "text/css; charset=utf-8", false),
        "/admin.css" => ("admin.css", "text/css; charset=utf-8", false),
        "/charts.js" => ("charts.js", "text/javascript", false),
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
        "/charts.js",
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

async fn static_file(dir: Arc<PathBuf>, uri: Uri) -> Response {
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
    let Ok(path) = tokio::fs::canonicalize(dir.join(file)).await else {
        return missing();
    };
    if !path.starts_with(dir.as_path()) {
        return missing();
    }
    let Ok(data) = tokio::fs::read(path).await else {
        return missing();
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, data.len())
        .header(
            header::CACHE_CONTROL,
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        )
        .body(Body::from(data))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;
    #[tokio::test]
    async fn static_assets_headers_and_traversal() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("static");
        let app = static_router(validate_static(&dir).await.unwrap());
        for path in [
            "/",
            "/admin",
            "/app.js",
            "/app.css",
            "/admin.js",
            "/admin.css",
            "/charts.js",
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
