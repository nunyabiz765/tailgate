mod app;

slint::include_modules!();

const HELP: &str = "\
Tailgate: manage Tailscale and its Mullvad exit nodes

USAGE
  tailgate [--read-only]

FLAGS
  --read-only   Show live status, but refuse every change before it runs
  --version     Print the version
  --help        Print this

ENVIRONMENT (for testing)
  TAILGATE_READ_ONLY=1       Same as --read-only
  TAILGATE_TAILSCALE_BIN     tailscale binary to use
  TAILGATE_PKEXEC_BIN        pkexec binary to use
  TAILGATE_OPEN_BIN          xdg-open binary to use
  TAILGATE_CONFIG_DIR        Directory for state.json
  TAILGATE_IPCHECK_URL       URL for Check my IP
";

fn main() {
    let mut read_only = std::env::var("TAILGATE_READ_ONLY").is_ok_and(|v| !v.is_empty() && v != "0");
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--read-only" => read_only = true,
            "--version" | "-V" => {
                println!("tailgate {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return;
            }
            other => {
                eprintln!("tailgate: unknown argument '{other}'. Try --help.");
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = app::run(read_only) {
        eprintln!("tailgate: couldn't open a window: {e}");
        std::process::exit(1);
    }
}
