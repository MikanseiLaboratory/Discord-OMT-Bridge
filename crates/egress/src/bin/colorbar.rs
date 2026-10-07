use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let seconds = args
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);
    println!("publishing OMT color bars for {seconds}s as \"Discord Colorbar\"");
    if let Err(err) = egress::run_colorbar(
        "Discord Colorbar",
        true,
        cfg!(feature = "ndi"),
        Duration::from_secs(seconds),
    ) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
