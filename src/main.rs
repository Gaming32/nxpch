use crate::output::{generate_ips, generate_pchtxt};
use crate::parse::{BuildTarget, ForcedBuildOption, parse_statements};
use crate::pchtxt::{pchtxt_to_nxpch, pchtxt_to_patches};
use crate::pre_parse::PreParsedCode;
use crate::preprocessor::MacroDefine;
use clap::{Parser, Subcommand};
use clap_stdin::{FileOrStdin, FileOrStdout};
use hashlink::LinkedHashSet;
use keystone::{Arch, Keystone, Mode};
use miette::{Context, Diagnostic, GraphicalReportHandler, IntoDiagnostic, NamedSource, Severity};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{iter, process};

mod option;
mod output;
mod parse;
mod pchtxt;
mod pre_parse;
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
        /// Generates a single file instead of a zip file distribution. Will error if multiple
        /// files would be generated. May be given a file path to output to.
        #[clap(short('S'), long)]
        single: Option<Option<PathBuf>>,
        /// Single build target to build for. If not specified, will build for both targets.
        #[clap(short, long)]
        target: Option<BuildTarget>,
        /// An additional predefined macro in the form of -DMACRO_NAME or -D"MACRO_NAME expansion".
        /// Note that this differs from the gcc syntax of -DMACRO_NAME=expansion. This may be
        /// specified multiple times.
        #[clap(short('D'), long)]
        define: Vec<MacroDefine>,
    },
    /// Convert a file from another format to nxpch
    #[clap(subcommand)]
    Import(ImportCommands),
    /// Commands related to working directly with pchtxt files
    #[clap(subcommand)]
    Pchtxt(PchtxtCommands),
}

#[derive(Subcommand)]
enum ImportCommands {
    /// Compile pchtxt to nxpch
    Pchtxt {
        /// The source pchtxt file
        source: FileOrStdin,
        /// The output nxpch file
        output: FileOrStdout,
    },
}

#[derive(Subcommand)]
enum PchtxtCommands {
    /// Compile pchtxt to ips
    Compile {
        /// The source pchtxt file
        source: FileOrStdin,
        /// The output ips file
        output: FileOrStdout,
    },
    /// Minify the pchtxt file to make it smaller
    Minify {
        /// The source pchtxt file
        source: FileOrStdin,
        /// The output pchtxt file
        output: FileOrStdout,
    },
}

fn main() -> miette::Result<()> {
    let args = RootArgs::parse();
    match args.command {
        Commands::Build {
            source,
            single: _,
            target,
            define,
        } => {
            let (filename, source, pre_parsed_statements) = parse_source_code(source, |src| {
                let parsed = PreParsedCode::parse(src);
                (parsed.statements, parsed.diagnostics)
            })?;

            let mut parse_diags = LinkedHashSet::new();
            let generated_results = parse_statements(
                pre_parsed_statements.into_iter().map(|(_, s)| s),
                define,
                target.map_or_else(
                    || vec![BuildTarget::Emulator, BuildTarget::Hardware],
                    |t| vec![t],
                ),
                ForcedBuildOption {
                    build_id: None,
                    options: vec![],
                },
                |diag| {
                    parse_diags.get_or_insert(diag);
                },
            );
            check_error_count(print_diags(parse_diags, &filename, &source));

            println!("{:#?}", generated_results);
        }
        Commands::Import(ImportCommands::Pchtxt { source, output }) => {
            let (_, _, nxpch) = parse_source_code(source, pchtxt_to_nxpch)?;
            let (out_filename, mut output) = open_file_or_stdout(output)?;
            output
                .write_all(nxpch.as_bytes())
                .into_diagnostic()
                .with_context(|| format!("File {out_filename}"))?;
            eprintln!("Finished importing into {out_filename}");
        }
        Commands::Pchtxt(PchtxtCommands::Compile { source, output }) => {
            let (_, _, (patch, _)) = parse_source_code(source, pchtxt_to_patches)?;
            let (out_filename, output) = open_file_or_stdout(output)?;
            generate_ips(&patch, output)?;
            eprintln!("Finished compiling to {out_filename}");
        }
        Commands::Pchtxt(PchtxtCommands::Minify { source, output }) => {
            let (_, _, (patch, bid)) = parse_source_code(source, pchtxt_to_patches)?;
            let (out_filename, output) = open_file_or_stdout(output)?;
            generate_pchtxt(&patch, bid, output)
                .into_diagnostic()
                .with_context(|| format!("Writing file {out_filename}"))?;
            eprintln!("Finished minifying to {out_filename}");
        }
    }
    Ok(())
}

fn parse_source_code<T, D>(
    source: FileOrStdin,
    parser: impl FnOnce(&str) -> (T, Vec<D>),
) -> miette::Result<(String, String, T)>
where
    D: Diagnostic + Send + Sync + 'static,
{
    let filename = if source.is_file() {
        source.filename()
    } else {
        "<stdin>"
    }
    .to_string();
    let raw_source = source
        .contents()
        .into_diagnostic()
        .with_context(|| format!("Reading file {filename}"))?;
    let (parsed, diags) = parser(&raw_source);
    check_error_count(print_diags(diags, &filename, &raw_source));
    Ok((filename, raw_source, parsed))
}

fn open_file_or_stdout(output: FileOrStdout) -> miette::Result<(String, BufWriter<impl Write>)> {
    let filename = if output.is_file() {
        output.filename()
    } else {
        "<stdout>"
    }
    .to_string();
    let writer = BufWriter::new(
        output
            .into_writer()
            .into_diagnostic()
            .with_context(|| format!("File {filename}"))?,
    );
    Ok((filename, writer))
}

fn print_diags(
    diags: impl IntoIterator<Item = impl Diagnostic + Send + Sync + 'static>,
    filename: &str,
    source: &str,
) -> usize {
    let mut diags = diags.into_iter();
    let Some(first) = diags.next() else {
        return 0;
    };
    let source_code = Arc::new(NamedSource::new(filename, source.to_string()));
    let reporter = GraphicalReportHandler::new();
    let mut message = String::new();
    let mut error_count = 0;
    for diag in iter::once(first).chain(diags) {
        if diag.severity().is_none_or(|x| x == Severity::Error) {
            error_count += 1;
        }
        let report = miette::Report::new(diag).with_source_code(source_code.clone());
        let _ = reporter.render_report(&mut message, &*report);
        eprintln!("{message}");
        message.clear();
    }
    error_count
}

fn check_error_count(error_count: usize) {
    if error_count > 0 {
        eprintln!("Build failed due to {error_count} error(s)");
        process::exit(error_count.try_into().unwrap_or(i32::MAX))
    }
}

fn fiddle() {
    let key = Keystone::new(Arch::ARM64, Mode::empty()).unwrap();
    let result = key
        .asm(
            r"
    informDamageFull_Sender = 9889896
    ldr x0, [x1, #0xdb0]
    b informDamageFull_Sender
    "
            .to_string(),
            0x0064E638,
        )
        .unwrap();
    for chunk in result.bytes.as_chunks::<4>().0 {
        println!("{:08X}", u32::from_be_bytes(*chunk));
    }
}
