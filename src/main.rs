mod backend;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use backend::{Error, Request};

/// Take screenshots on anything running a Linux kernel.
#[derive(Parser)]
#[command(name = "rustshot", version)]
struct Cli {
    /// Let the desktop show its own picker, if the chosen backend has one
    #[arg(short, long)]
    interactive: bool,

    /// Output file (default: `screenshot_YYYYMMDD_HHMMSS.png`)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Force a backend instead of autodetecting
    #[arg(short, long, value_name = "NAME")]
    backend: Option<String>,

    /// Report which backends this build has and whether each one applies here
    #[arg(long)]
    list_backends: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_backends {
        list_backends();
        return ExitCode::SUCCESS;
    }

    match run(&cli) {
        Ok(path) => {
            println!("saved -> {}", path.display());
            ExitCode::SUCCESS
        }
        Err(Error::Cancelled) => {
            eprintln!("cancelled");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<PathBuf, Error> {
    let backend = match &cli.backend {
        Some(name) => backend::detect_named(name).map_err(Error::Other)?,
        None => backend::detect().map_err(|declined| {
            use std::fmt::Write as _;
            let mut msg = String::from("no capture backend applies to this system:");
            for (name, reason) in declined {
                let _ = write!(msg, "\n  {name}: {reason}");
            }
            Error::Other(msg)
        })?,
    };

    // Fail before capturing rather than quietly returning something other than
    // what was asked for.
    if cli.interactive && !backend.caps().interactive {
        return Err(Error::Unsupported(format!(
            "the {} backend has no picker; drop --interactive",
            backend.name()
        )));
    }

    let request = Request {
        interactive: cli.interactive,
    };
    let capture = backend.capture(&request)?;

    let dest = cli.output.clone().unwrap_or_else(backend::default_output);
    capture.save(&dest)?;
    eprintln!("[{}]", backend.describe());
    Ok(dest)
}

fn list_backends() {
    for (name, status) in backend::survey() {
        match status {
            Ok(detail) => println!("  {name:<8} available  — {detail}"),
            Err(reason) => println!("  {name:<8} no         — {reason}"),
        }
    }
}
