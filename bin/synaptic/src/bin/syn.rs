// `syn` is a short alias for the `synaptic` binary; both share run_cli().
fn main() -> anyhow::Result<()> {
    synaptic_cli::run_cli()
}
