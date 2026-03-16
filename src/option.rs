use crate::preprocessor::MacroDefine;
use json5_nodes::JsonNode;
use miette::SourceOffset;
use serde::{Deserialize, Serialize};
use strsim::damerau_levenshtein;
use thiserror::Error;

#[derive(Clone, Debug)]
pub enum NxpchOption {
    TargetBuild(TargetBuildOption),
    TargetBuilds(TargetBuildsOption),
    PointerOffset(PointerOffsetOption),
    UserSettings(UserSettingsOption),
    OutputFormat(OutputFormatOption),
}

impl NxpchOption {
    pub fn parse(option_name: &str, content: &str) -> Result<Self, OptionParseError> {
        Ok(match option_name {
            "target_build" => NxpchOption::TargetBuild(json5::from_str(content)?),
            "target_builds" => NxpchOption::TargetBuilds(json5::from_str(content)?),
            "pointer_offset" => NxpchOption::PointerOffset(json5::from_str(content)?),
            "user_settings" => NxpchOption::UserSettings(json5::from_str(content)?),
            "output_format" => NxpchOption::OutputFormat(json5::from_str(content)?),
            _ => {
                return Err(OptionParseError::UnknownOption {
                    closest: [
                        "target_build",
                        "target_builds",
                        "pointer_offset",
                        "user_settings",
                        "output_format",
                    ]
                    .into_iter()
                    .min_by_key(|x| damerau_levenshtein(x, option_name))
                    .unwrap(),
                });
            }
        })
    }

    /// Requires that `json` is the same as the `content` used in `parse`
    pub fn update_offsets(&mut self, json: &str, start_offset: usize) {
        macro_rules! mismatch {
            () => {
                panic!("update_offsets json is mismatched from parse content")
            };
        }
        match self {
            Self::TargetBuild(_) => {}
            Self::TargetBuilds(entries) => {
                let nodes = match json5_nodes::parse(json) {
                    Ok(JsonNode::Array(root, _)) => root,
                    _ => mismatch!(),
                };
                for (entry, node) in entries.0.iter_mut().zip(nodes) {
                    let defines_node = match node {
                        JsonNode::Object(mut obj, _) => match obj.remove("defines") {
                            Some(JsonNode::Array(defines, _)) => defines,
                            None => vec![],
                            _ => mismatch!(),
                        },
                        _ => mismatch!(),
                    };
                    for (define, node) in entry.defines.iter_mut().zip(defines_node) {
                        let location = match node {
                            JsonNode::String(_, loc) => loc,
                            _ => mismatch!(),
                        };
                        if let Some(location) = location {
                            let offset = start_offset
                                + SourceOffset::from_location(json, location.line, location.column)
                                    .offset()
                                + 1;
                            define.declaration_range.0 += offset;
                            define.expansion_offset += offset;
                        }
                    }
                }
            }
            Self::PointerOffset(_) => {}
            Self::UserSettings(settings) => {
                let nodes = match json5_nodes::parse(json) {
                    Ok(JsonNode::Array(root, _)) => root,
                    _ => mismatch!(),
                };
                for (setting, node) in settings.0.iter_mut().zip(nodes) {
                    let defines_node = match node {
                        JsonNode::Object(mut obj, _) => match obj.remove("defines") {
                            Some(JsonNode::Array(defines, _)) => defines,
                            None => vec![],
                            _ => mismatch!(),
                        },
                        _ => mismatch!(),
                    };
                    for (define, node) in setting.defines.iter_mut().zip(defines_node) {
                        let location = match node {
                            JsonNode::String(_, loc) => loc,
                            _ => mismatch!(),
                        };
                        if let Some(location) = location {
                            let offset = start_offset
                                + SourceOffset::from_location(json, location.line, location.column)
                                    .offset()
                                + 1;
                            define.declaration_range.0 += offset;
                            define.expansion_offset += offset;
                        }
                    }
                }
            }
            Self::OutputFormat(_) => {}
        }
    }
}

#[derive(Debug, Error)]
pub enum OptionParseError {
    #[error(transparent)]
    InvalidOption(#[from] json5::Error),
    #[error("Unknown option")]
    UnknownOption { closest: &'static str },
}

pub type BuildId = u128;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetBuildOption(BuildId);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetBuildsOption(Vec<TargetBuildMatrixEntry>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetBuildMatrixEntry {
    pub id: BuildId,
    pub defines: Vec<MacroDefine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PointerOffsetOption(i32);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSettingsOption(Vec<UserSetting>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSetting {
    pub layer: u8,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defines: Vec<MacroDefine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputFormatOption(OutputFormat);

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Ips,
    Pchtxt,
}
