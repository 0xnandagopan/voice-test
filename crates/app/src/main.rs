use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use sqlx::postgres::PgPoolOptions;
use std::{
    env,
    io::{self, Read},
};
use v0_app::{AppState, config::Config, router, with_static};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    v0_app::runtime::initialize_tls();
    v0_app::config::load_dotenv()?;
    if env::args().nth(1).as_deref() == Some("hash-password") {
        let mut password = String::new();
        io::stdin().read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\n', '\r']);
        if password.len() < 12 {
            return Err("Password must contain at least 12 characters".into());
        }
        println!(
            "{}",
            Argon2::default()
                .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
                .map_err(|_| "Could not hash password")?
        );
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be configured")?;
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;
    if env::args().nth(1).as_deref() == Some("migrate") {
        sqlx::migrate!("../../migrations").run(&pool).await?;
        tracing::info!("Database migrations applied");
        return Ok(());
    }
    let config = Config::from_env()?;
    // Validate private storage configuration before accepting traffic. This does
    // not probe remote access; the synthetic bucket gate verifies that separately.
    let _storage = v0_evidence::storage::storage_from_env().await?;
    let address = v0_app::runtime::bind_address(
        env::var("BIND_ADDR").ok().as_deref(),
        env::var("PORT").ok().as_deref(),
    )?;
    if !config.secure_cookie && !address.ip().is_loopback() {
        return Err("Insecure development cookies require a loopback bind".into());
    }
    let mut state = AppState::new(pool, config);
    if env::var("VOICE_TEST_ENABLED").as_deref() == Ok("true") {
        state = state.with_controlled_voice(
            env::var("VOICE_PUBLIC_ORIGIN")
                .map_err(|_| "VOICE_PUBLIC_ORIGIN is required for controlled voice")?,
        )?;
    }
    let app = router(state);
    let app = match env::var("SERVE_WEB").as_deref().unwrap_or("true") {
        "true" => with_static(
            app,
            &env::var("WEB_DIST").unwrap_or_else(|_| "web/dist".into()),
        ),
        "false" => app,
        _ => return Err("SERVE_WEB must be true or false".into()),
    };
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(address=%listener.local_addr()?, "Application listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(v0_app::runtime::shutdown_signal())
        .await?;
    Ok(())
}
