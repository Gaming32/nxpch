use crate::parse::PreParsedCode;
use clap::{Parser, Subcommand};
use clap_stdin::FileOrStdin;
use miette::{Context, Diagnostic, GraphicalReportHandler, IntoDiagnostic, NamedSource, Severity};
use std::process;
use std::sync::Arc;

mod macros;
mod option;
mod output;
mod parse;
mod preprocessor;
mod utils;

#[derive(Parser)]
struct RootArgs {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a nxpch into a mod distribution
    Build {
        /// The mod source code
        source: FileOrStdin,
    },
}

fn main() -> miette::Result<()> {
    let args = RootArgs::parse();
    match args.command {
        Commands::Build { source } => {
            let filename = source.filename().to_string();
            let raw_source = source
                .contents()
                .into_diagnostic()
                .with_context(|| format!("File {filename}"))?;
            let pre_parsed = PreParsedCode::parse(&raw_source);
            let build_failure_count = pre_parsed
                .diagnostics
                .iter()
                .filter(|x| x.severity().is_none_or(|x| x == Severity::Error))
                .count();
            print_diags(pre_parsed.diagnostics, &filename, &raw_source);
            if build_failure_count > 0 {
                process::exit(build_failure_count.try_into().unwrap_or(i32::MAX));
            }
            println!("{:#?}", pre_parsed.statements);
        }
    }
    Ok(())
}

fn print_diags(diags: Vec<impl Diagnostic + Send + Sync + 'static>, filename: &str, source: &str) {
    if diags.is_empty() {
        return;
    }
    let source_code = Arc::new(NamedSource::new(filename, source.to_string()));
    let reporter = GraphicalReportHandler::new();
    let mut message = String::new();
    for diag in diags {
        let report = miette::Report::new(diag).with_source_code(source_code.clone());
        let _ = reporter.render_report(&mut message, &*report);
        println!("{message}");
        message.clear();
    }
}
