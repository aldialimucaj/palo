use palo_core::domain::AppState;
use palo_core::orchestration::Orchestrator;
use palo_tui::logging::init_tui_tracing;
use palo_tui::run_with_orchestrator;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    init_tui_tracing(Some(std::env::temp_dir().join("palo-tui.log")));

    let orchestrator = Orchestrator::new(AppState::default());
    run_with_orchestrator(&orchestrator).await
}
