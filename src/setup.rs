use crate::config::valid_launchd_label;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const XML_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
"#;

#[derive(Debug)]
pub enum SetupError {
    InvalidLabel,
    PathNotAbsolute { name: &'static str, path: PathBuf },
    PathNotUtf8 { name: &'static str, path: PathBuf },
    InvalidXmlCharacter { name: &'static str, character: char },
    LogDirectoryUnavailable { path: PathBuf, source: io::Error },
    LogDirectoryNotDirectory { path: PathBuf },
}

impl Display for SetupError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLabel => write!(formatter, "launchd label is invalid"),
            Self::PathNotAbsolute { name, path } => {
                write!(
                    formatter,
                    "{name} path must be absolute: {}",
                    path.display()
                )
            }
            Self::PathNotUtf8 { name, path } => {
                write!(
                    formatter,
                    "{name} path is not valid UTF-8: {}",
                    path.display()
                )
            }
            Self::InvalidXmlCharacter { name, character } => write!(
                formatter,
                "{name} contains an XML 1.0-incompatible character (U+{:04X})",
                u32::from(*character)
            ),
            Self::LogDirectoryUnavailable { path, source } => write!(
                formatter,
                "cannot inspect log directory {}: {source}",
                path.display()
            ),
            Self::LogDirectoryNotDirectory { path } => {
                write!(
                    formatter,
                    "log directory path is not a directory: {}",
                    path.display()
                )
            }
        }
    }
}

impl Error for SetupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LogDirectoryUnavailable { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn gateway_plist_xml(
    label: &str,
    program: &Path,
    config: &Path,
    log_dir: &Path,
) -> Result<String, SetupError> {
    if !valid_launchd_label(label) {
        return Err(SetupError::InvalidLabel);
    }
    validate_xml_string("launchd label", label)?;
    let program = validated_path("program", program)?;
    let config = validated_path("configuration", config)?;
    validated_path("log directory", log_dir)?;
    let metadata = fs::metadata(log_dir).map_err(|source| SetupError::LogDirectoryUnavailable {
        path: log_dir.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(SetupError::LogDirectoryNotDirectory {
            path: log_dir.to_path_buf(),
        });
    }

    // Label validation excludes '/', so each derived name stays within log_dir.
    let stdout_log_path = log_dir.join(format!("{label}.stdout.log"));
    let stderr_log_path = log_dir.join(format!("{label}.stderr.log"));
    let stdout_log = validated_path("standard output log", &stdout_log_path)?;
    let stderr_log = validated_path("standard error log", &stderr_log_path)?;

    let plist = PropertyListValue::Dictionary(vec![
        ("Label", PropertyListValue::String(label)),
        (
            "ProgramArguments",
            PropertyListValue::Array(vec![
                PropertyListValue::String(program),
                PropertyListValue::String(config),
            ]),
        ),
        ("RunAtLoad", PropertyListValue::Bool(true)),
        ("KeepAlive", PropertyListValue::Bool(true)),
        ("StandardOutPath", PropertyListValue::String(stdout_log)),
        ("StandardErrorPath", PropertyListValue::String(stderr_log)),
    ]);

    let mut xml = String::from(XML_PREFIX);
    plist.write_xml(&mut xml, 0);
    xml.push_str("</plist>\n");
    Ok(xml)
}

fn validated_path<'a>(name: &'static str, path: &'a Path) -> Result<&'a str, SetupError> {
    if !path.is_absolute() {
        return Err(SetupError::PathNotAbsolute {
            name,
            path: path.to_path_buf(),
        });
    }
    let value = path.to_str().ok_or_else(|| SetupError::PathNotUtf8 {
        name,
        path: path.to_path_buf(),
    })?;
    validate_xml_string(name, value)?;
    Ok(value)
}

fn validate_xml_string(name: &'static str, value: &str) -> Result<(), SetupError> {
    if let Some(character) = value
        .chars()
        .find(|character| !is_xml_1_0_character(*character))
    {
        return Err(SetupError::InvalidXmlCharacter { name, character });
    }
    Ok(())
}

fn is_xml_1_0_character(character: char) -> bool {
    matches!(
        character,
        '\u{9}' | '\u{A}' | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

enum PropertyListValue<'a> {
    Dictionary(Vec<(&'a str, PropertyListValue<'a>)>),
    Array(Vec<PropertyListValue<'a>>),
    String(&'a str),
    Bool(bool),
}

impl PropertyListValue<'_> {
    fn write_xml(&self, output: &mut String, indentation: usize) {
        match self {
            Self::Dictionary(entries) => {
                write_indentation(output, indentation);
                output.push_str("<dict>\n");
                for (key, value) in entries {
                    write_indentation(output, indentation + 1);
                    output.push_str("<key>");
                    write_escaped_xml(output, key);
                    output.push_str("</key>\n");
                    value.write_xml(output, indentation + 1);
                }
                write_indentation(output, indentation);
                output.push_str("</dict>\n");
            }
            Self::Array(values) => {
                write_indentation(output, indentation);
                output.push_str("<array>\n");
                for value in values {
                    value.write_xml(output, indentation + 1);
                }
                write_indentation(output, indentation);
                output.push_str("</array>\n");
            }
            Self::String(value) => {
                write_indentation(output, indentation);
                output.push_str("<string>");
                write_escaped_xml(output, value);
                output.push_str("</string>\n");
            }
            Self::Bool(value) => {
                write_indentation(output, indentation);
                output.push_str(if *value { "<true/>\n" } else { "<false/>\n" });
            }
        }
    }
}

fn write_indentation(output: &mut String, indentation: usize) {
    for _ in 0..indentation {
        output.push_str("  ");
    }
}

fn write_escaped_xml(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_deterministic_gateway_plist_and_escapes_every_string() {
        let label = "net.example.&<>\"'";
        let program = Path::new("/Applications/Roused & <Tools>/roused\"'");
        let config = Path::new("/Users/example/Roused & <Config>/roused\"'.toml");
        let log_dir = Path::new("/tmp");

        let first = gateway_plist_xml(label, program, config, log_dir).unwrap();
        let second = gateway_plist_xml(label, program, config, log_dir).unwrap();

        assert_eq!(first, second);
        assert!(first.contains("<string>net.example.&amp;&lt;&gt;&quot;&apos;</string>"));
        assert!(first.contains(
            "<string>/Applications/Roused &amp; &lt;Tools&gt;/roused&quot;&apos;</string>"
        ));
        assert!(first.contains(
            "<string>/Users/example/Roused &amp; &lt;Config&gt;/roused&quot;&apos;.toml</string>"
        ));
        assert!(
            first
                .contains("<string>/tmp/net.example.&amp;&lt;&gt;&quot;&apos;.stdout.log</string>")
        );
        assert!(
            first
                .contains("<string>/tmp/net.example.&amp;&lt;&gt;&quot;&apos;.stderr.log</string>")
        );
        assert!(first.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(first.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(first.contains("<key>StandardOutPath</key>"));
        assert!(first.contains("<key>StandardErrorPath</key>"));
    }

    #[test]
    fn rejects_invalid_labels_and_paths() {
        assert!(matches!(
            gateway_plist_xml(
                "bad label",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml"),
                Path::new("/tmp"),
            ),
            Err(SetupError::InvalidLabel)
        ));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("relative/roused"),
                Path::new("/tmp/a.toml"),
                Path::new("/tmp"),
            ),
            Err(SetupError::PathNotAbsolute {
                name: "program",
                ..
            })
        ));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("relative/a.toml"),
                Path::new("/tmp"),
            ),
            Err(SetupError::PathNotAbsolute {
                name: "configuration",
                ..
            })
        ));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml"),
                Path::new("relative/logs"),
            ),
            Err(SetupError::PathNotAbsolute {
                name: "log directory",
                ..
            })
        ));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/bad\u{1}.toml"),
                Path::new("/tmp"),
            ),
            Err(SetupError::InvalidXmlCharacter {
                name: "configuration",
                character: '\u{1}',
            })
        ));
    }

    #[test]
    fn rejects_missing_and_non_directory_log_paths() {
        let directory = tempfile::tempdir().expect("create setup log-directory test directory");
        let missing = directory.path().join("missing");
        let regular_file = directory.path().join("regular-file");
        fs::write(&regular_file, b"not a directory\n").expect("write regular-file fixture");

        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml"),
                &missing,
            ),
            Err(SetupError::LogDirectoryUnavailable { path, .. }) if path == missing
        ));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml"),
                &regular_file,
            ),
            Err(SetupError::LogDirectoryNotDirectory { path }) if path == regular_file
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let program = PathBuf::from(OsString::from_vec(b"/tmp/roused-\xff".to_vec()));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                &program,
                Path::new("/tmp/a.toml"),
                Path::new("/tmp"),
            ),
            Err(SetupError::PathNotUtf8 {
                name: "program",
                ..
            })
        ));

        let log_dir = PathBuf::from(OsString::from_vec(b"/tmp/roused-logs-\xff".to_vec()));
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml"),
                &log_dir,
            ),
            Err(SetupError::PathNotUtf8 {
                name: "log directory",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_a_log_directory_symlink_without_canonicalizing_it() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("create symlink test directory");
        let real_log_dir = directory.path().join("real-logs");
        let selected_log_dir = directory.path().join("stable-logs");
        fs::create_dir(&real_log_dir).expect("create real log directory");
        symlink(&real_log_dir, &selected_log_dir).expect("create log directory symlink");

        let plist = gateway_plist_xml(
            "net.example.roused",
            Path::new("/bin/roused"),
            Path::new("/tmp/a.toml"),
            &selected_log_dir,
        )
        .expect("generate plist through log-directory symlink");

        assert!(
            plist.contains(
                &selected_log_dir
                    .join("net.example.roused.stderr.log")
                    .display()
                    .to_string()
            )
        );
        assert!(
            !plist.contains(
                &real_log_dir
                    .join("net.example.roused.stderr.log")
                    .display()
                    .to_string()
            )
        );
    }
}
