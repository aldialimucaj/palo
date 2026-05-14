use palo_core::orchestration::Orchestrator;
use tracing::info;

pub mod app;
pub mod event;
pub mod logging;
pub mod ui;

pub async fn build_app(orchestrator: &Orchestrator) -> app::App {
    app::App::from_orchestrator(orchestrator).await
}

pub async fn run_app(app: app::App) -> color_eyre::Result<()> {
    let terminal = ratatui::init();
    let result = app.run(terminal).await;
    ratatui::restore();
    info!("palo tui session ended");
    result
}

pub async fn run_with_orchestrator(orchestrator: &Orchestrator) -> color_eyre::Result<()> {
    run_app(build_app(orchestrator).await).await
}
