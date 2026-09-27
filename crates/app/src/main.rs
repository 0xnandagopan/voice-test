use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use sqlx::postgres::PgPoolOptions;
use std::{
    env,
    io::{self, Read},
    net::SocketAddr,
};
use v0_app::{AppState, config::Config, router, with_static};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
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
    let address: SocketAddr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()?;
    if !config.secure_cookie && !address.ip().is_loopback() {
        return Err("Insecure development cookies require a loopback bind".into());
    }
    let app = with_static(
        router(AppState::new(pool, config)),
        &env::var("WEB_DIST").unwrap_or_else(|_| "web/dist".into()),
    );
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(address=%listener.local_addr()?, "Application listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
