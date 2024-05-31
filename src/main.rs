use clap::{Command, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Generator, Shell};
use log::error;
use std::io;
use std::process::exit;

pub mod cmd;
pub mod fmap;

#[derive(Parser)]
#[command(version, about, long_about = None, arg_required_else_help = true)]
pub struct Cli {
    // If provided, outputs the completion file for given shell
    #[arg(long = "generate", value_enum)]
    generator: Option<Shell>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(alias("dump_fmap"), disable_help_flag = true)]
    /// Dump FlashMap (FMAP) layout or sections.
    DumpFmap(cmd::dump_fmap::DumpFmapArgs),

    #[command()]
    ExtractFmap(cmd::extract_fmap::ExtractFmapArgs),

    #[command(alias("load_fmap"))]
    LoadFmap(cmd::load_fmap::LoadFmapArgs),
}

fn print_completions<G: Generator>(gen: G, cmd: &mut Command) {
    generate(gen, cmd, cmd.get_name().to_string(), &mut io::stdout());
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    if let Some(generator) = cli.generator {
        let mut cmd = Cli::command();
        eprintln!("Generating completion file for {generator:?}...");
        print_completions(generator, &mut cmd);
        exit(0);
    }
    let command = cli
        .command
        .as_ref()
        .expect("empty command should not be allowed by parser");
    let result = match command {
        Commands::DumpFmap(args) => cmd::dump_fmap::run_command(args),
        Commands::ExtractFmap(args) => cmd::extract_fmap::run_command(args),
        Commands::LoadFmap(args) => cmd::load_fmap::run_command(args),
    };

    if let Err(e) = result {
        error!("{}", e);
        exit(-1);
    }
}
