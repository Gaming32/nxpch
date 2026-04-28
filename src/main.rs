use crate::assemble::Assembler;
use crate::option::OutputFormat;
use crate::output::{check_generate_ips, generate_ips, generate_pchtxt};
use crate::parse::{BuildTarget, ForcedBuildOption, parse_statements};
use crate::pchtxt::{pchtxt_to_nxpch, pchtxt_to_patches};
use crate::pre_parse::PreParsedCode;
use crate::preprocessor::MacroDefine;
use crate::zip_gen::{PathSegment, generate_zip, generate_zip_filename};
use clap::{Parser, Subcommand};
use clap_stdin::{FileOrStdin, FileOrStdout};
use indexmap::IndexSet;
use miette::{
    Context, Diagnostic, GraphicalReportHandler, IntoDiagnostic, NamedSource, Severity, miette,
};
use std::fmt::Display;
use std::fs::File;
use std::io::{BufWriter, Write, stdout};
use std::iter;
use std::path::Path;
use std::sync::Arc;
use tempfile::NamedTempFile;

mod assemble;
mod option;
mod output;
mod parse;
mod pchtxt;
mod pre_parse;
mod preprocessor;
mod utils;
mod zip_gen;

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
        #[clap(short, long)]
        single: Option<Option<FileOrStdout>>,
        /// Single build target to build for. If not specified, will build for both targets.
        #[clap(short('T'), long)]
        target: Option<BuildTarget>,
        /// Set a value to a user_setting. This arg may be specified multiple times, and it must be
        /// specified in the order of the options in the nxpch source, and the values must match
        /// the names exactly as they are defined.
        #[clap(short('S'), long)]
        setting: Vec<String>,
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
    miette::set_panic_hook();
    let args = RootArgs::parse();
    match args.command {
        Commands::Build {
            source,
            single,
            target,
            setting,
            define,
        } => {
            let (filename, source, pre_parsed_statements) = parse_source_code(source, |src| {
                let parsed = PreParsedCode::parse(src);
                (parsed.statements, parsed.diagnostics)
            })?;

            let mut parse_diags = IndexSet::new();
            let generated_results = parse_statements(
                pre_parsed_statements.into_iter().map(|(_, s)| s),
                define,
                target.map_or_else(
                    || vec![BuildTarget::Emulator, BuildTarget::Hardware],
                    |t| vec![t],
                ),
                ForcedBuildOption {
                    build_id: None,
                    options: setting,
                },
                |diag| {
                    parse_diags.insert(diag);
                },
            );
            check_error_count(print_diags(parse_diags, &filename, &source))?;

            let (mut emulator, mut hardware): (Vec<_>, _) = generated_results
                .into_iter()
                .partition(|x| x.build_target == BuildTarget::Emulator);
            if let Some(single_path) = single {
                fn generate_count_error(count: usize) -> miette::Result<()> {
                    print_error_and_exit(
                        format_args!(
                            "--single was used, but {count} outputs were generated (expected 1).",
                        ),
                        Some(
                            "Consider using --target, --build, and --setting to reduce output count.",
                        ),
                    )
                }
                match (emulator.len(), hardware.len()) {
                    (1, 1) => {
                        hardware[0].build_target = BuildTarget::Emulator;
                        if hardware[0] != emulator[0] {
                            generate_count_error(2)?;
                        }
                        drop(hardware);
                    }
                    (1, 0) => {}
                    (0, 1) => emulator = hardware,
                    _ => generate_count_error(emulator.len() + hardware.len())?,
                }

                let to_build = emulator.into_iter().next().unwrap();
                let mut compile_diags = vec![];
                let built = Assembler::new().assemble(
                    Arc::try_unwrap(to_build.code).unwrap(),
                    to_build.labels,
                    |diag| {
                        compile_diags.push(diag);
                    },
                );
                check_error_count(print_diags(compile_diags, &filename, &source))?;

                let write_to_path_prefix = |prefix| -> miette::Result<_> {
                    if check_generate_ips(&built).is_ok() {
                        let path = format!("{prefix}.ips");
                        generate_ips(&built, open_file(&path)?)
                            .with_context(|| format!("{WRITING_FILE}{path}"))?;
                    } else {
                        let path = format!("{prefix}.pchtxt");
                        generate_pchtxt(&built, to_build.target_build, open_file(&path)?)
                            .into_diagnostic()
                            .with_context(|| format!("{WRITING_FILE}{path}"))?;
                    }
                    Ok(())
                };
                if let Some(path) = single_path {
                    if path.is_file() {
                        let path = path.filename();
                        match path
                            .rsplit_once('.')
                            .map(|(_, ext)| ext.to_lowercase())
                            .as_deref()
                        {
                            Some("ips") => generate_ips(&built, open_file(path)?)
                                .with_context(|| format!("{WRITING_FILE}{path}"))?,
                            Some("pchtxt") => {
                                generate_pchtxt(&built, to_build.target_build, open_file(path)?)
                                    .into_diagnostic()
                                    .with_context(|| format!("{WRITING_FILE}{path}"))?
                            }
                            Some(ext) => print_error_and_exit(
                                format_args!(
                                    "Unknown file extension for --single writing .{ext}. Please use .ips or .pchtxt.",
                                ),
                                None,
                            )?,
                            None => write_to_path_prefix(path)?,
                        }
                    } else {
                        generate_pchtxt(&built, to_build.target_build, stdout())
                            .into_diagnostic()
                            .with_context(|| format!("{WRITING_FILE}{STDOUT}"))?;
                    }
                } else if filename != STDIN {
                    write_to_path_prefix(
                        filename
                            .rsplit_once('.')
                            .map_or(&filename, |(stem, _)| stem),
                    )?;
                } else {
                    generate_pchtxt(&built, to_build.target_build, stdout())
                        .into_diagnostic()
                        .with_context(|| format!("{WRITING_FILE}{STDOUT}"))?;
                }
            } else {
                let mut diags = vec![];
                let mut record_diagnostic = |diag| diags.push(diag);
                let emulator_temp = if !emulator.is_empty() {
                    println!("Generating emulator zip");
                    let mut temp = NamedTempFile::new()
                        .into_diagnostic()
                        .with_context(|| "Creating emulator zip tempfile")?;
                    let filename = generate_zip_filename(&emulator, "emulator")
                        .map_err(&mut record_diagnostic)
                        .ok();
                    generate_zip(
                        emulator,
                        temp.as_file_mut(),
                        &[PathSegment::ModName, PathSegment::Static("exefs")],
                        PathSegment::BuildId,
                        false,
                        &mut record_diagnostic,
                    )
                    .into_diagnostic()
                    .with_context(|| "Generating emulator zip")?;
                    filename.map(|name| (temp, name))
                } else {
                    None
                };
                let hardware_temp = if !hardware.is_empty() {
                    println!("Generating hardware zip");
                    let mut temp = NamedTempFile::new()
                        .into_diagnostic()
                        .with_context(|| "Creating hardware zip tempfile")?;
                    let filename = generate_zip_filename(&hardware, "hardware")
                        .map_err(&mut record_diagnostic)
                        .ok();
                    let path_segments = if hardware
                        .iter()
                        .all(|r| r.forced_output_format == Some(OutputFormat::Pchtxt))
                    {
                        &[
                            PathSegment::Static("switch"),
                            PathSegment::Static("ipswitch"),
                            PathSegment::ModName,
                        ] as &[_]
                    } else {
                        &[
                            PathSegment::Static("atmosphere"),
                            PathSegment::Static("exefs_patches"),
                            PathSegment::ModName,
                        ]
                    };
                    generate_zip(
                        hardware,
                        temp.as_file_mut(),
                        path_segments,
                        PathSegment::BuildId,
                        true,
                        &mut record_diagnostic,
                    )
                    .into_diagnostic()
                    .with_context(|| "Generating hardware zip")?;
                    filename.map(|name| (temp, name))
                } else {
                    None
                };
                check_error_count(print_diags(diags, &filename, &source))?;

                let output_dir = if filename == STDIN {
                    Path::new("")
                } else {
                    let file_path = Path::new(filename.as_str());
                    if let Some(parent) = file_path.parent() {
                        parent
                    } else {
                        Path::new("")
                    }
                };
                let generate_output = |temp: Option<(NamedTempFile, _)>| -> miette::Result<()> {
                    if let Some((temp, filename)) = temp {
                        let out_path = output_dir.join(filename);
                        temp.persist(&out_path)
                            .into_diagnostic()
                            .with_context(|| format!("{WRITING_FILE}{}", out_path.display()))?;
                        println!("Wrote {}", out_path.display());
                    }
                    Ok(())
                };
                generate_output(emulator_temp)?;
                generate_output(hardware_temp)?;
            }
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
                .with_context(|| format!("{WRITING_FILE}{out_filename}"))?;
            eprintln!("Finished minifying to {out_filename}");
        }
    }
    Ok(())
}

pub const STDIN: &str = "<stdin>";
pub const STDOUT: &str = "<stdout>";
pub const READING_FILE: &str = "Reading file ";
pub const WRITING_FILE: &str = "Writing file ";

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
        STDIN
    }
    .to_string();
    let raw_source = source
        .contents()
        .into_diagnostic()
        .with_context(|| format!("{READING_FILE}{filename}"))?;
    let (parsed, diags) = parser(&raw_source);
    check_error_count(print_diags(diags, &filename, &raw_source))?;
    Ok((filename, raw_source, parsed))
}

fn open_file_or_stdout(output: FileOrStdout) -> miette::Result<(String, BufWriter<impl Write>)> {
    let filename = if output.is_file() {
        output.filename()
    } else {
        STDOUT
    }
    .to_string();
    let writer = BufWriter::new(
        output
            .into_writer()
            .into_diagnostic()
            .with_context(|| format!("{WRITING_FILE}{filename}"))?,
    );
    Ok((filename, writer))
}

fn open_file(path: impl AsRef<Path>) -> miette::Result<BufWriter<File>> {
    Ok(BufWriter::new(
        File::create(path.as_ref())
            .into_diagnostic()
            .with_context(|| format!("{WRITING_FILE}{}", path.as_ref().display()))?,
    ))
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

fn check_error_count(error_count: usize) -> miette::Result<()> {
    if error_count > 0 {
        Err(miette!("Build failed due to {error_count} error(s)"))
    } else {
        Ok(())
    }
}

fn print_error_and_exit(error: impl Display, tip: Option<&str>) -> miette::Result<()> {
    if let Some(help) = tip {
        Err(miette!(help = help, "{error}"))
    } else {
        Err(miette!("{error}"))
    }
}
