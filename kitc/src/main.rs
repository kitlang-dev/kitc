use clap::{Parser, Subcommand};
use kitlang::codegen::frontend::Compiler;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time;

type Error = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(name = "kitc", version, about = "Kit compiler")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // TODO: the compiler artifacts should be deleted as soon as the final C program has been
    // successfully compiled. There should be a flag to disable this behavior.
    /// Compile a .kit file to an executable
    Compile {
        /// The `.kit` source file
        source: PathBuf,

        /// Add a source path (format: dir or dir:prefix). Can be repeated.
        #[arg(short = 'p', long = "source-path")]
        source_paths: Vec<String>,

        /// The libraries to link against
        #[arg(short, long)]
        libs: Vec<String>,

        /// Compile and immediately run the executable
        #[arg(long)]
        run: bool,

        /// The output file name
        #[arg(long)]
        measure: bool,
    },
}

fn main() -> Result<(), Error> {
    env_logger::init();

    // Destructure the Cli to get the `command` field
    let Cli { command } = Cli::parse();

    match command {
        Commands::Compile {
            source,
            source_paths,
            libs,
            run,
            measure,
        } => {
            if !source.exists() {
                eprintln!(
                    "{} does not exist. Please check again the path and try again.",
                    source.display()
                );
                return Ok(());
            }

            let exe_path = compile(&source, &source_paths, &libs, measure)?;
            if run {
                run_executable(&exe_path)?;
            } else {
                println!("→ Successfully compiled!");
            }
        }
    }
    Ok(())
}

fn compile(
    source: &Path,
    source_paths: &[String],
    libs: &[String],
    measure: bool,
) -> Result<PathBuf, String> {
    let init = time::Instant::now();

    let ext = if cfg!(windows) { "exe" } else { "" };
    let exe_path = source.with_extension(ext);

    let mut compiler = Compiler::new(
        vec![source.to_path_buf()],
        &exe_path,
        libs.to_vec(),
        source_paths.to_vec(),
    );

    compiler
        .compile()
        .map_err(|e| format!("failed to compile: {e}"))?;

    if measure {
        println!("→ Compiled in {}ms", init.elapsed().as_millis());
    }

    Ok(exe_path)
}

// TODO: return the exit status from the compiler code, and return Err() if it failed, probably
// adding an exit status (to exit with).
fn run_executable(exe_path: &Path) -> Result<(), String> {
    let status = Command::new(exe_path)
        .status()
        .map_err(|e| format!("failed to launch executable: {e}"))?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
