use donkey::{Config, run};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 显式安装 CryptoProvider：依赖树里 aws-lc-rs 与 ring 并存时，
    // rustls 无法自动决定用哪个，首次 TLS 握手会 panic。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("donkey=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    run(Config::from_env()?).await
}
