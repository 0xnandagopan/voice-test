use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use v0_app::{AppState, config::Config, router, with_static};

const APP: &str = "https://voice-test-app.trypreview.online";

fn application() -> Router {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    router(AppState::new(
        pool,
        Config {
            origin: APP.into(),
            agency_name: "Synthetic agency".into(),
            operator_username: "operator".into(),
            operator_password_hash: "unused".into(),
            invitation_signing_key: "synthetic-key-not-used-by-these-tests".into(),
            secure_cookie: true,
            voice_api_key: None,
        },
    ))
}

#[tokio::test]
async fn split_origin_preflight_allows_only_the_configured_frontend() {
    for origin in [
        APP,
        "https://attacker.example",
        "https://voice-test-app.trypreview.online.attacker.example",
        "null",
    ] {
        let response = application()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/operator/login")
                    .header(header::ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        if origin == APP {
            assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], APP);
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
                "true"
            );
            assert!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
                    .to_str()
                    .unwrap()
                    .contains("content-type")
            );
        } else {
            // The fixed allowed origin never reflects an untrusted request Origin.
            assert_ne!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|v| v.to_str().ok()),
                Some(origin)
            );
        }
    }
}

#[tokio::test]
async fn cors_does_not_replace_mutation_authorization_or_hide_allowed_origin_errors() {
    for origin in [None, Some("https://attacker.example")] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/customer/consent")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let response = application()
            .oneshot(request.body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("invalid_origin"));
    }
    let response = application()
        .oneshot(
            Request::builder()
                .uri("/api/operator/me")
                .header(header::ORIGIN, APP)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], APP);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn local_spa_refresh_succeeds_without_masking_unknown_api_routes() {
    let directory = std::env::temp_dir().join(format!("slug-spa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&directory).await.unwrap();
    tokio::fs::write(
        directory.join("index.html"),
        "<html>synthetic app shell</html>",
    )
    .await
    .unwrap();
    let app = with_static(application(), directory.to_str().unwrap());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/operator")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
            .contains("synthetic app shell")
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[test]
fn cookies_remain_host_only_secure_and_http_only_for_same_site_subdomains() {
    let cookie = v0_app::auth::session_cookie("customer_session", "synthetic", true, 600);
    let value = cookie.to_str().unwrap();
    assert!(
        value.contains("HttpOnly") && value.contains("SameSite=Strict") && value.contains("Secure")
    );
    assert!(!value.contains("Domain="));
}
