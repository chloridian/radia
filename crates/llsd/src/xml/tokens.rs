use std::{borrow::Cow, str};

use quick_xml::{
    Reader,
    escape::resolve_predefined_entity,
    events::{BytesRef, BytesStart, Event},
};

use super::is_xml_char;
use crate::{Error, Limits, Result};

const MAX_BINARY_ATTRIBUTES: usize = 4096;

#[derive(Debug)]
pub(super) enum Token<'a> {
    Start(&'static str),
    Empty(&'static str),
    End(&'static str),
    Text(Cow<'a, str>),
    CharacterReference(char),
}

pub(super) struct Tokens<'input> {
    reader: Reader<&'input [u8]>,
    pending: Option<Token<'input>>,
    preserve_text: Vec<bool>,
    limits: Limits,
    count: usize,
    ended: bool,
}

impl<'input> Tokens<'input> {
    pub(super) fn new(input: &'input [u8], limits: Limits) -> Result<Self> {
        let input = str::from_utf8(input).map_err(|_| Error::InvalidUtf8)?;
        if input.chars().any(|character| !is_xml_char(character)) {
            return Err(Error::InvalidValue("input contains a character forbidden in XML 1.0".into()));
        }
        let mut reader = Reader::from_str(input);
        reader.config_mut().trim_text(false);
        Ok(Self { reader, pending: None, preserve_text: Vec::new(), limits, count: 0, ended: false })
    }

    pub(super) fn peek(&mut self) -> Result<Option<&Token<'input>>> {
        if self.pending.is_none() && !self.ended {
            self.pending = self.read()?;
        }
        Ok(self.pending.as_ref())
    }

    pub(super) fn take(&mut self) -> Result<Option<Token<'input>>> {
        self.peek()?;
        Ok(self.pending.take())
    }

    fn check_next_name(&self) -> Result<()> {
        let remaining = self.reader.get_ref().strip_prefix(b"\xef\xbb\xbf").unwrap_or(self.reader.get_ref());
        let Some(markup) = remaining.strip_prefix(b"<") else {
            return Ok(());
        };
        if markup.starts_with(b"!") || markup.starts_with(b"?") {
            return Ok(());
        }
        let name = markup.strip_prefix(b"/").unwrap_or(markup);
        let len = name.iter().position(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')).unwrap_or(name.len());
        tag_name(str::from_utf8(&name[..len]).map_err(|_| Error::InvalidUtf8)?)?;
        Ok(())
    }

    fn count_event(&mut self) -> Result<()> {
        if self.count >= self.limits.max_values.saturating_mul(8).saturating_add(16) {
            return Err(Error::LimitExceeded("XML token count"));
        }
        self.count += 1;
        Ok(())
    }

    fn read(&mut self) -> Result<Option<Token<'input>>> {
        loop {
            self.check_next_name()?;
            let event = self
                .reader
                .read_event()
                .map_err(|_| Error::InvalidValue(format!("malformed XML at byte {}", self.reader.error_position())))?;
            self.count_event()?;
            let token = match event {
                Event::Start(element) => {
                    let name = tag_name(element.name().as_ref())?;
                    if name == "binary" {
                        validate_binary_encoding(&element)?;
                    }
                    self.preserve_text.push(!matches!(name, "llsd" | "array" | "map"));
                    Token::Start(name)
                }
                Event::Empty(element) => {
                    let name = tag_name(element.name().as_ref())?;
                    if name == "binary" {
                        validate_binary_encoding(&element)?;
                    }
                    Token::Empty(name)
                }
                Event::End(element) => {
                    self.preserve_text.pop();
                    Token::End(tag_name(element.name().as_ref())?)
                }
                Event::Text(text) => Token::Text(text.into_inner()),
                Event::GeneralRef(reference) => {
                    let character = reference
                        .resolve_char_ref()
                        .map_err(|_| Error::InvalidValue("invalid XML character reference".into()))?
                        .or_else(|| resolve_predefined_entity(reference.as_ref()).and_then(|text| text.chars().next()))
                        .ok_or_else(|| Error::InvalidValue("unknown XML entity".into()))?;
                    if !is_xml_char(character) {
                        return Err(Error::InvalidValue("character reference is forbidden in XML 1.0".into()));
                    }
                    Token::CharacterReference(character)
                }
                Event::CData(text) => Token::Text(text.into_inner()),
                Event::DocType(_) => continue,
                Event::Eof => {
                    self.ended = true;
                    return Ok(None);
                }
                Event::Decl(_) | Event::PI(_) | Event::Comment(_) => continue,
            };
            let preserve_text = self.preserve_text.last().copied().unwrap_or_default();
            if !preserve_text {
                match &token {
                    Token::Text(value) if value.bytes().all(|byte| byte.is_ascii_whitespace()) => continue,
                    Token::CharacterReference(character) if character.is_ascii_whitespace() => continue,
                    _ => {}
                }
            }
            return Ok(Some(token));
        }
    }
}

fn tag_name(name: &str) -> Result<&'static str> {
    match name {
        "llsd" => Ok("llsd"),
        "array" => Ok("array"),
        "map" => Ok("map"),
        "key" => Ok("key"),
        "undef" => Ok("undef"),
        "boolean" => Ok("boolean"),
        "integer" => Ok("integer"),
        "real" => Ok("real"),
        "uuid" => Ok("uuid"),
        "string" => Ok("string"),
        "uri" => Ok("uri"),
        "date" => Ok("date"),
        "binary" => Ok("binary"),
        _ => Err(Error::InvalidValue("unknown LLSD XML element".into())),
    }
}

fn validate_binary_encoding(element: &BytesStart<'_>) -> Result<()> {
    for (index, attribute) in element.attributes().with_checks(true).enumerate() {
        if index >= MAX_BINARY_ATTRIBUTES {
            return Err(Error::LimitExceeded("XML attribute count"));
        }
        let attribute = attribute.map_err(|_| Error::InvalidValue("malformed LLSD binary attribute".into()))?;
        if attribute.key.as_ref() != "encoding" {
            continue;
        }

        if !base64_encoding(attribute.value.as_ref()) {
            return Err(Error::InvalidValue("unsupported LLSD binary encoding".into()));
        }
    }
    Ok(())
}

fn base64_encoding(mut text: &str) -> bool {
    for expected in "base64".chars() {
        if let Some(remaining) = text.strip_prefix(expected) {
            text = remaining;
            continue;
        }
        let Some(reference) = text.strip_prefix('&') else {
            return false;
        };
        let Some((reference, remaining)) = reference.split_once(';') else {
            return false;
        };
        if !BytesRef::new(reference).resolve_char_ref().is_ok_and(|character| character == Some(expected)) {
            return false;
        }
        text = remaining;
    }
    text.is_empty()
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, borrow::Cow};

    use super::{Token, Tokens};
    use crate::Limits;

    #[test]
    fn unescaped_text_tokens_borrow_input() {
        let input = b"<llsd><string>ordinary text</string></llsd>";
        let mut stream = Tokens::new(input, Limits::default()).expect("read XML");
        let mut tokens = Vec::new();
        while let Some(token) = stream.take().expect("read token") {
            tokens.push(token);
        }
        let text = tokens.iter().find_map(|token| match token {
            Token::Text(text) => Some(text),
            _ => None,
        });

        assert_matches!(text, Some(Cow::Borrowed("ordinary text")));
    }
}
