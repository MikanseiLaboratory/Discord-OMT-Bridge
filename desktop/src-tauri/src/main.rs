fn main() {
    if discord_omt_bridge::maybe_run_av1_worker() {
        return;
    }
    discord_omt_desktop::init_tracing();
    discord_omt_desktop::run();
}
