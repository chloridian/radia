use std::{
    borrow::Cow,
    fs::File,
    io::{self, Read},
    path::Path,
    str,
};

use cvars::{CvarDescriptor, CvarError, CvarValue, ValueDomain, lookup_by_name};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, Value, value};

use super::SettingsError;

pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;
pub const MAX_SETTINGS_TABLES: usize = 64;
pub const MAX_SETTINGS_KEYS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileFingerprint(Option<[u8; 32]>);

impl FileFingerprint {
    pub(super) const ABSENT: Self = Self(None);

    pub(super) fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        Self(Some(hash))
    }
}

pub(super) struct ParsedDocument {
    pub(super) document: DocumentMut,
    pub(super) preset: Option<String>,
    pub(super) entries: Vec<(String, CvarValue)>,
    pub(super) unknown_keys: Vec<String>,
}

pub(super) fn read_bounded(path: &Path) -> Result<Option<(Vec<u8>, FileFingerprint)>, SettingsError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SettingsError::Io(error.to_string())),
    };
    let mut bytes = Vec::new();
    let limit = (MAX_SETTINGS_BYTES + 1) as u64;
    file.take(limit).read_to_end(&mut bytes).map_err(|error| SettingsError::Io(error.to_string()))?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::FileTooLarge { max_bytes: MAX_SETTINGS_BYTES });
    }
    let fingerprint = FileFingerprint::from_bytes(&bytes);
    Ok(Some((bytes, fingerprint)))
}

pub(super) fn fingerprint_on_disk(path: &Path) -> Result<FileFingerprint, SettingsError> {
    match read_bounded(path)? {
        Some((_, fingerprint)) => Ok(fingerprint),
        None => Ok(FileFingerprint::ABSENT),
    }
}

pub(super) fn parse_document(bytes: &[u8]) -> Result<ParsedDocument, SettingsError> {
    let text = str::from_utf8(bytes).map_err(|error| SettingsError::Parse(error.to_string()))?;
    let document = text.parse::<DocumentMut>().map_err(|error| SettingsError::Parse(error.to_string()))?;
    let mut preset = None;
    let mut entries = Vec::new();
    let mut unknown_keys = Vec::new();
    let mut table_count = 0usize;
    let mut key_count = 0usize;

    for (key, item) in document.iter() {
        match item {
            Item::Table(table) => {
                walk_table(key, table, &mut table_count, &mut key_count, &mut entries, &mut unknown_keys)?;
            }
            Item::Value(raw) => {
                key_count += 1;
                if key_count > MAX_SETTINGS_KEYS {
                    return Err(SettingsError::TooManyKeys { max_keys: MAX_SETTINGS_KEYS });
                }
                if key == "graphics_preset" {
                    let Some(name) = raw.as_str() else {
                        return Err(SettingsError::InvalidValue(CvarError::TypeMismatch { name: "graphics_preset", expected: "string" }));
                    };
                    preset = Some(name.to_owned());
                } else {
                    unknown_keys.push(key.to_owned());
                }
            }
            Item::ArrayOfTables(_) | Item::None => {
                unknown_keys.push(key.to_owned());
            }
        }
    }

    Ok(ParsedDocument { document, preset, entries, unknown_keys })
}

fn walk_table(
    prefix: &str,
    table: &Table,
    table_count: &mut usize,
    key_count: &mut usize,
    entries: &mut Vec<(String, CvarValue)>,
    unknown_keys: &mut Vec<String>,
) -> Result<(), SettingsError> {
    *table_count += 1;
    if *table_count > MAX_SETTINGS_TABLES {
        return Err(SettingsError::TooManyTables { max_tables: MAX_SETTINGS_TABLES });
    }
    for (key, item) in table.iter() {
        let dotted = format!("{prefix}.{key}");
        match item {
            Item::Table(nested) => {
                walk_table(&dotted, nested, table_count, key_count, entries, unknown_keys)?;
            }
            Item::Value(raw) => {
                *key_count += 1;
                if *key_count > MAX_SETTINGS_KEYS {
                    return Err(SettingsError::TooManyKeys { max_keys: MAX_SETTINGS_KEYS });
                }
                if let Some(descriptor) = lookup_by_name(&dotted) {
                    let parsed = parse_cvar_value(descriptor, raw)?;
                    entries.push((dotted, parsed));
                } else {
                    unknown_keys.push(dotted);
                }
            }
            Item::ArrayOfTables(_) | Item::None => {
                unknown_keys.push(dotted);
            }
        }
    }
    Ok(())
}

fn parse_cvar_value(descriptor: &'static CvarDescriptor, raw: &Value) -> Result<CvarValue, SettingsError> {
    let converted = match (descriptor.domain, raw) {
        (ValueDomain::Bool, Value::Boolean(flag)) => CvarValue::Bool(*flag.value()),
        (ValueDomain::I64 { .. }, Value::Integer(number)) => CvarValue::I64(*number.value()),
        (ValueDomain::F32 { .. }, Value::Float(number)) => CvarValue::F32(*number.value() as f32),
        (ValueDomain::F32 { .. }, Value::Integer(number)) => CvarValue::F32(*number.value() as f32),
        (ValueDomain::F64 { .. }, Value::Float(number)) => CvarValue::F64(*number.value()),
        (ValueDomain::F64 { .. }, Value::Integer(number)) => CvarValue::F64(*number.value() as f64),
        (ValueDomain::String { .. } | ValueDomain::Enum { .. }, Value::String(text)) => CvarValue::String(Cow::Owned(text.value().clone())),
        _ => {
            return Err(SettingsError::InvalidValue(CvarError::TypeMismatch {
                name: descriptor.name,
                expected: descriptor.domain.type_name(),
            }));
        }
    };
    descriptor.domain.validate(descriptor.name, &converted).map_err(SettingsError::InvalidValue)
}

pub(super) fn set_document_value(document: &mut DocumentMut, descriptor: &'static CvarDescriptor, next: Option<&CvarValue>) {
    let segments = descriptor.name.split('.').collect::<Vec<_>>();
    let Some((leaf, parents)) = segments.split_last() else {
        return;
    };
    match next {
        Some(cvar_value) => {
            let mut current = document.as_table_mut();
            for segment in parents {
                if !current.contains_key(segment) {
                    let mut table = Table::new();
                    table.set_implicit(false);
                    current.insert(segment, Item::Table(table));
                }
                let Some(table) = current.get_mut(segment).and_then(Item::as_table_mut) else {
                    return;
                };
                current = table;
            }
            current.insert(leaf, cvar_value_to_item(cvar_value));
        }
        None => {
            remove_key_preserving_comments(document.as_table_mut(), parents, leaf);
        }
    }
}

fn remove_key_preserving_comments(table: &mut Table, parents: &[&str], leaf: &str) {
    if let Some((first, rest)) = parents.split_first() {
        let should_prune = if let Some(child) = table.get_mut(first).and_then(Item::as_table_mut) {
            remove_key_preserving_comments(child, rest, leaf);
            child.is_empty() && !table_has_comments(child)
        } else {
            false
        };
        if should_prune {
            table.remove(first);
        }
    } else {
        table.remove(leaf);
    }
}

fn table_has_comments(table: &Table) -> bool {
    let prefix_has = table.decor().prefix().and_then(|raw| raw.as_str()).is_some_and(|text| text.contains('#'));
    let suffix_has = table.decor().suffix().and_then(|raw| raw.as_str()).is_some_and(|text| text.contains('#'));
    prefix_has || suffix_has
}

fn cvar_value_to_item(cvar_value: &CvarValue) -> Item {
    match cvar_value {
        CvarValue::Bool(flag) => value(*flag),
        CvarValue::I64(number) => value(*number),
        CvarValue::F32(number) => value(f64::from(*number)),
        CvarValue::F64(number) => value(*number),
        CvarValue::String(text) => value(text.as_ref()),
        CvarValue::Enum(variant) => value(*variant),
    }
}
