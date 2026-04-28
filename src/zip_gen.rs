use crate::assemble::{AssembleDiagnostic, Assembler};
use crate::option::OutputFormat;
use crate::output::{IpsGenerateError, check_generate_ips, generate_ips, generate_pchtxt};
use crate::parse::ParsingResult;
use miette::Diagnostic;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::io::{BufWriter, Seek, Write};
use std::sync::Arc;
use thiserror::Error;
use zip::result::ZipResult;
use zip::write::FileOptions;

pub fn generate_zip(
    patches: Vec<ParsingResult>,
    output: impl Write + Seek,
    dir_segments: &[PathSegment],
    file_name: PathSegment,
    force_ips_default: bool,
    mut record_diagnostic: impl FnMut(ZipGenerateDiagnostic),
) -> ZipResult<()> {
    let mut zip = zip::ZipWriter::new(BufWriter::new(output)).set_auto_large_file();

    let mut made_directories = HashSet::new();
    let assembler = Assembler::new();

    let all_force_pchtxt = patches
        .iter()
        .all(|r| r.forced_output_format == Some(OutputFormat::Pchtxt));

    let mut built_path = String::new();
    for patch in patches {
        built_path.clear();
        for option in patch.user_settings.iter() {
            built_path.push_str(option);
            built_path.push('/');
            if !made_directories.contains(&built_path) {
                made_directories.insert(built_path.clone());
                zip.add_directory(&built_path, FileOptions::DEFAULT)?;
            }
        }
        for segment in dir_segments {
            segment.format(&patch, &mut built_path);
            built_path.push('/');
            if !made_directories.contains(&built_path) {
                made_directories.insert(built_path.clone());
                zip.add_directory(&built_path, FileOptions::DEFAULT)?;
            }
        }
        file_name.format(&patch, &mut built_path);

        let compiled = assembler.assemble(patch.code.iter().cloned(), patch.labels, |diag| {
            record_diagnostic(diag.into());
        });
        let forces_pchtxt = patch.forced_output_format == Some(OutputFormat::Pchtxt);
        let ips_generate_error = (!forces_pchtxt)
            .then(|| check_generate_ips(&compiled).err())
            .flatten();

        if force_ips_default && !all_force_pchtxt {
            if let Some(err) = ips_generate_error {
                record_diagnostic(ZipGenerateDiagnostic::PchtxtRequired {
                    settings: Arc::unwrap_or_clone(patch.user_settings),
                    cause: err,
                });
                continue;
            } else if forces_pchtxt {
                record_diagnostic(ZipGenerateDiagnostic::PchtxtPartiallyRequested {
                    settings: Arc::unwrap_or_clone(patch.user_settings),
                });
                continue;
            }
        }

        let should_generate_ips = match patch.forced_output_format {
            Some(OutputFormat::Ips) => {
                if let Some(err) = ips_generate_error {
                    record_diagnostic(ZipGenerateDiagnostic::IpsError {
                        settings: Arc::unwrap_or_clone(patch.user_settings),
                        cause: err,
                    });
                    continue;
                }
                true
            }
            Some(OutputFormat::Pchtxt) => false,
            None => ips_generate_error.is_none(),
        };
        if should_generate_ips {
            built_path.push_str(".ips");
        } else {
            built_path.push_str(".pchtxt");
        }
        zip.start_file(&built_path, FileOptions::DEFAULT)?;
        if should_generate_ips {
            generate_ips(&compiled, &mut zip).map_err(|e| e.unwrap_io_err())?;
        } else {
            generate_pchtxt(&compiled, patch.target_build, &mut zip)?;
        }
    }

    zip.finish().and_then(|mut out| Ok(out.flush()?))
}

#[derive(Copy, Clone, Debug)]
pub enum PathSegment {
    Static(&'static str),
    ModName,
    BuildId,
}

impl PathSegment {
    fn format(self, patch: &ParsingResult, output: &mut String) {
        match self {
            PathSegment::Static(text) => output.push_str(text),
            PathSegment::ModName => output.push_str("TEST"), // TODO: Mod name
            PathSegment::BuildId => {
                let _ = write!(output, "{:X}", patch.target_build);
            }
        }
    }
}

pub fn generate_zip_filename(
    _from: &[ParsingResult],
    target: &str,
) -> Result<String, ZipGenerateDiagnostic> {
    Ok(format!("TEST+{target}.zip")) // TODO: Mod name + version
}

#[derive(Debug, Error, Diagnostic)]
pub enum ZipGenerateDiagnostic {
    #[error("The user setting {settings:?} requires pchtxt while building for real hardware.")]
    #[diagnostic(
        code(zip::pchtxt_required),
        help(
            "Include the original exefs's byte immediately preceding this address in your nxpch with the `.byte` directive (ideally surrounded by `#if HARDWARE`). Alternatively, force pchtxt output with `output_format = \"pchtxt\"`. However, forcing pchtxt is not recommended, as pchtxt has a worse user experience on real hardware."
        )
    )]
    PchtxtRequired {
        settings: Vec<Arc<str>>,

        #[source]
        #[diagnostic_source]
        cause: IpsGenerateError,
    },

    #[error(
        "The user setting {settings:?} requested pchtxt on real hardware, but other settings use IPS."
    )]
    #[diagnostic(
        code(zip::pchtxt_partially_requested),
        help(
            "Force pchtxt globally by using `output_format = \"pchtxt\"` at the top. You can also surround it with `#if HARDWARE` to make it only apply to real hardware."
        )
    )]
    PchtxtPartiallyRequested { settings: Vec<Arc<str>> },

    #[error("The user setting {settings:?} forced IPS format, which generated an error.")]
    #[diagnostic(code(zip::ips_error))]
    IpsError {
        settings: Vec<Arc<str>>,

        #[source]
        #[diagnostic_source]
        cause: IpsGenerateError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Assemble(#[from] AssembleDiagnostic),
}
