use crate::config::valid_launchd_label;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

const XML_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
"#;

#[derive(Debug, Eq, PartialEq)]
pub enum SetupError {
    InvalidLabel,
    PathNotAbsolute { name: &'static str, path: PathBuf },
    PathNotUtf8 { name: &'static str, path: PathBuf },
    InvalidXmlCharacter { name: &'static str, character: char },
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
        }
    }
}

impl Error for SetupError {}

pub fn gateway_plist_xml(label: &str, program: &Path, config: &Path) -> Result<String, SetupError> {
    if !valid_launchd_label(label) {
        return Err(SetupError::InvalidLabel);
    }
    validate_xml_string("launchd label", label)?;
    let program = validated_path("program", program)?;
    let config = validated_path("configuration", config)?;

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

        let first = gateway_plist_xml(label, program, config).unwrap();
        let second = gateway_plist_xml(label, program, config).unwrap();

        assert_eq!(first, second);
        assert!(first.contains("<string>net.example.&amp;&lt;&gt;&quot;&apos;</string>"));
        assert!(first.contains(
            "<string>/Applications/Roused &amp; &lt;Tools&gt;/roused&quot;&apos;</string>"
        ));
        assert!(first.contains(
            "<string>/Users/example/Roused &amp; &lt;Config&gt;/roused&quot;&apos;.toml</string>"
        ));
        assert!(first.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(first.contains("<key>KeepAlive</key>\n  <true/>"));
    }

    #[test]
    fn rejects_invalid_labels_and_paths() {
        assert_eq!(
            gateway_plist_xml(
                "bad label",
                Path::new("/bin/roused"),
                Path::new("/tmp/a.toml")
            )
            .unwrap_err(),
            SetupError::InvalidLabel
        );
        assert!(matches!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("relative/roused"),
                Path::new("/tmp/a.toml")
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
                Path::new("relative/a.toml")
            ),
            Err(SetupError::PathNotAbsolute {
                name: "configuration",
                ..
            })
        ));
        assert_eq!(
            gateway_plist_xml(
                "net.example.roused",
                Path::new("/bin/roused"),
                Path::new("/tmp/bad\u{1}.toml")
            )
            .unwrap_err(),
            SetupError::InvalidXmlCharacter {
                name: "configuration",
                character: '\u{1}',
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let program = PathBuf::from(OsString::from_vec(b"/tmp/roused-\xff".to_vec()));
        assert!(matches!(
            gateway_plist_xml("net.example.roused", &program, Path::new("/tmp/a.toml")),
            Err(SetupError::PathNotUtf8 {
                name: "program",
                ..
            })
        ));
    }
}
