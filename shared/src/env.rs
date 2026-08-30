//! A dotenv reader for the values the badge needs at build time.
//!
//! The firmware cannot use the SDK's `envconfig`: that feature pulls in
//! `dirs`, which exists to locate `~/.config`, and ESP-IDF has no such notion.
//! `firmware/build.rs` therefore parses the file itself and bakes the result
//! in as constants, which is why this parser stays even though `web/` moved
//! its own credential loading onto the SDK.

use std::{collections::HashMap, error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for EnvParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl Error for EnvParseError {}

pub fn parse_env(content: &str) -> Result<HashMap<String, String>, EnvParseError> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            Some((index + 1, line.strip_prefix("export ").unwrap_or(line)))
        })
        .map(|(line_number, line)| {
            let (key, raw_value) = line.split_once('=').ok_or_else(|| EnvParseError {
                line: line_number,
                message: "expected KEY=VALUE".to_owned(),
            })?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
            {
                return Err(EnvParseError {
                    line: line_number,
                    message: format!("invalid environment key {key:?}"),
                });
            }
            Ok((
                key.to_owned(),
                parse_env_value(raw_value.trim(), line_number)?,
            ))
        })
        .collect()
}

fn parse_env_value(value: &str, line: usize) -> Result<String, EnvParseError> {
    let Some(quote) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '\'' | '"'))
    else {
        let comment = value.char_indices().find_map(|(index, character)| {
            (character == '#'
                && (index == 0
                    || value[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace)))
            .then_some(index)
        });
        return Ok(value[..comment.unwrap_or(value.len())].trim().to_owned());
    };

    let mut escaped = false;
    let mut closing_quote = None;
    for (index, character) in value.char_indices().skip(1) {
        if quote == '"' && escaped {
            escaped = false;
            continue;
        }
        if quote == '"' && character == '\\' {
            escaped = true;
        } else if character == quote {
            closing_quote = Some(index);
            break;
        }
    }
    let closing_quote = closing_quote.ok_or_else(|| EnvParseError {
        line,
        message: "unterminated quoted value".to_owned(),
    })?;
    let remainder = value[closing_quote + quote.len_utf8()..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Err(EnvParseError {
            line,
            message: "unexpected text after quoted value".to_owned(),
        });
    }
    let quoted = &value[quote.len_utf8()..closing_quote];
    if quote == '\'' {
        return Ok(quoted.to_owned());
    }
    let mut parsed = String::with_capacity(quoted.len());
    let mut characters = quoted.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }
        let escaped = characters.next().ok_or_else(|| EnvParseError {
            line,
            message: "trailing escape in quoted value".to_owned(),
        })?;
        parsed.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            '"' => '"',
            other => other,
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_common_dotenv_syntax() {
        let values = parse_env(
            "export ONE=plain # comment\nTWO='hash # stays'\nTHREE=\"line\\nvalue\" # comment",
        )
        .unwrap();
        assert_eq!(values["ONE"], "plain");
        assert_eq!(values["TWO"], "hash # stays");
        assert_eq!(values["THREE"], "line\nvalue");
        assert!(parse_env("BROKEN='unterminated").is_err());
    }

    #[test]
    fn an_unquoted_hash_without_leading_space_stays_in_the_value() {
        let values = parse_env("TAG=build#42\nNOTE=value # comment").unwrap();
        assert_eq!(values["TAG"], "build#42");
        assert_eq!(values["NOTE"], "value");
    }
}
