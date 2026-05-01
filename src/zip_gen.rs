use crate::assemble::{AssembleDiagnostic, Assembler};
use crate::option::OutputFormat;
use crate::output::{IpsGenerateError, check_generate_ips, generate_ips, generate_pchtxt};
use crate::parse::{ParsingResult, SettingsVec};
use miette::Diagnostic;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::cell::RefCell;
use std::collections::{HashSet, LinkedList};
use std::fmt::Write as FmtWrite;
use std::io::{BufWriter, IntoInnerError, Seek, Write};
use std::mem;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use zip::result::ZipResult;
use zip::write::FileOptions;
use zip::{DateTime, System};

const REPRO_FILE_OPTIONS: FileOptions<()> = FileOptions::DEFAULT
    .last_modified_time(DateTime::DEFAULT)
    .system(System::Unix);

pub fn generate_zip(
    patches: Vec<ParsingResult>,
    fallback_filename: &str,
    output: impl Write + Seek,
    dir_segments: &[PathSegment],
    file_name: PathSegment,
    force_ips_default: bool,
    record_diagnostic: &Mutex<impl FnMut(ZipGenerateDiagnostic) + Send>,
) -> ZipResult<()> {
    let all_force_pchtxt = patches
        .iter()
        .all(|r| r.forced_output_format == Some(OutputFormat::Pchtxt));

    let record_diagnostic = |diag| record_diagnostic.lock().unwrap()(diag);
    let all_assembled: LinkedList<_> = patches
        .into_par_iter()
        .map(|mut patch| {
            thread_local! {
                static ASSEMBLER: RefCell<Assembler> = RefCell::new(Assembler::new());
            }
            let compiled = ASSEMBLER.with_borrow(|assembler| {
                assembler.assemble(
                    patch.code.iter().cloned(),
                    mem::take(&mut patch.labels),
                    |diag| {
                        record_diagnostic(diag.into());
                    },
                )
            });
            (patch, compiled)
        })
        .collect();

    let mut zip = BufWriter::new(zip::ZipWriter::new(BufWriter::new(output)).set_auto_large_file());

    let mut built_path = String::new();
    let mut made_directories = HashSet::new();
    for (patch, compiled) in all_assembled {
        built_path.clear();
        for option in patch.user_settings.iter() {
            built_path.push_str(option);
            built_path.push('/');
            if !made_directories.contains(&built_path) {
                made_directories.insert(built_path.clone());
                zip.get_mut()
                    .add_directory(&built_path, REPRO_FILE_OPTIONS)?;
            }
        }
        for segment in dir_segments {
            segment.format(&patch, fallback_filename, &mut built_path);
            built_path.push('/');
            if !made_directories.contains(&built_path) {
                made_directories.insert(built_path.clone());
                zip.get_mut()
                    .add_directory(&built_path, REPRO_FILE_OPTIONS)?;
            }
        }
        file_name.format(&patch, fallback_filename, &mut built_path);

        let forces_pchtxt = patch.forced_output_format == Some(OutputFormat::Pchtxt);
        let ips_generate_error = (!forces_pchtxt)
            .then(|| check_generate_ips(&compiled).err())
            .flatten();
        if force_ips_default && !all_force_pchtxt {
            if let Some(err) = ips_generate_error {
                record_diagnostic(ZipGenerateDiagnostic::PchtxtRequired {
                    settings: patch.user_settings,
                    cause: err,
                });
                continue;
            } else if forces_pchtxt {
                record_diagnostic(ZipGenerateDiagnostic::PchtxtPartiallyRequested {
                    settings: patch.user_settings,
                });
                continue;
            }
        }

        let should_generate_ips = match patch.forced_output_format {
            Some(OutputFormat::Ips) => {
                if let Some(err) = ips_generate_error {
                    record_diagnostic(ZipGenerateDiagnostic::IpsError {
                        settings: patch.user_settings,
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

        zip.get_mut().start_file(&built_path, REPRO_FILE_OPTIONS)?;
        if should_generate_ips {
            generate_ips(&compiled, &mut zip).map_err(|e| e.unwrap_io_err())?;
        } else {
            generate_pchtxt(&compiled, patch.target_build, &mut zip)?;
        }
        zip.flush()?;
    }

    zip.into_inner()
        .map_err(IntoInnerError::into_error)?
        .finish()
        .and_then(|mut out| Ok(out.flush()?))
}

#[derive(Copy, Clone, Debug)]
pub enum PathSegment {
    Static(&'static str),
    ModName,
    BuildId,
}

impl PathSegment {
    fn format(self, patch: &ParsingResult, fallback_filename: &str, output: &mut String) {
        match self {
            PathSegment::Static(text) => output.push_str(text),
            PathSegment::ModName => {
                output.push_str(patch.mod_name.as_deref().unwrap_or(fallback_filename))
            }
            PathSegment::BuildId => {
                let _ = write!(output, "{:X}", patch.target_build);
            }
        }
    }
}

pub fn generate_zip_filename(
    from: &[ParsingResult],
    fallback_filename: &str,
    target: &str,
    record_diagnostic: &Mutex<impl FnMut(ZipGenerateDiagnostic) + Send>,
) -> Result<String, ZipGenerateDiagnostic> {
    fn find_equal_value<'a, T: Copy + PartialEq>(
        from: &'a [ParsingResult],
        record_diagnostic: &Mutex<impl FnMut(ZipGenerateDiagnostic) + Send>,
        getter: impl Fn(&'a ParsingResult) -> T,
        gen_diag: impl Fn(T, T, SettingsVec) -> ZipGenerateDiagnostic,
    ) -> T {
        let mut iter = from.iter();
        let value = getter(iter.next().unwrap());
        for remaining in iter {
            let other_value = getter(remaining);
            if other_value != value {
                record_diagnostic.lock().unwrap()(gen_diag(
                    value,
                    other_value,
                    remaining.user_settings.clone(),
                ));
            }
        }
        value
    }

    let mod_name = find_equal_value(
        from,
        record_diagnostic,
        |p| &p.mod_name,
        |first, second, second_settings| ZipGenerateDiagnostic::InconsistentModNames {
            first_name: first.clone().unwrap_or_else(|| "<unspecified>".into()),
            second_name: second.clone().unwrap_or_else(|| "<unspecified>".into()),
            second_settings,
        },
    )
    .as_deref()
    .unwrap_or(fallback_filename);
    let mod_version = find_equal_value(
        from,
        record_diagnostic,
        |p| &p.mod_version,
        |first, second, second_settings| ZipGenerateDiagnostic::InconsistentModVersions {
            first_version: first.clone(),
            second_version: second.clone(),
            second_settings,
        },
    );
    Ok(format!("{mod_name}-{mod_version}+{target}.zip"))
}

#[derive(Debug, Error, Diagnostic)]
pub enum ZipGenerateDiagnostic {
    #[error("The generated mod names differ, the first will be used.")]
    #[diagnostic(
        code(zip::inconsistent_mod_names),
        severity(warn),
        help(
            "The first name encountered was {first_name:?}. The inconsistency occurred under the setting {second_settings:?} with the name {second_name:?}."
        )
    )]
    InconsistentModNames {
        first_name: Arc<str>,
        second_name: Arc<str>,
        second_settings: SettingsVec,
    },

    #[error("The generated mod versions differ, the first will be used.")]
    #[diagnostic(
        code(zip::inconsistent_mod_versions),
        severity(warn),
        help(
            "The first version encountered was {first_version:?}. The inconsistency occurred under the setting {second_settings:?} with the version {second_version:?}."
        )
    )]
    InconsistentModVersions {
        first_version: Arc<str>,
        second_version: Arc<str>,
        second_settings: SettingsVec,
    },

    #[error("The user setting {settings:?} requires pchtxt while building for real hardware.")]
    #[diagnostic(
        code(zip::pchtxt_required),
        help(
            "Include the original exefs's byte immediately preceding this address in your nxpch with the `.byte` directive (ideally surrounded by `#if HARDWARE`). Alternatively, force pchtxt output with `output_format = \"pchtxt\"`. However, forcing pchtxt is not recommended, as pchtxt has a worse user experience on real hardware."
        )
    )]
    PchtxtRequired {
        settings: SettingsVec,

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
    PchtxtPartiallyRequested { settings: SettingsVec },

    #[error("The user setting {settings:?} forced IPS format, which generated an error.")]
    #[diagnostic(code(zip::ips_error))]
    IpsError {
        settings: SettingsVec,

        #[source]
        #[diagnostic_source]
        cause: IpsGenerateError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Assemble(#[from] AssembleDiagnostic),
}
