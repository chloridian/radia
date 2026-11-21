use std::{
    collections::BTreeSet,
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum ValueType {
    Bool,
    I64,
    F32,
    F64,
    String,
    Enum,
}

struct Declaration {
    name: String,
    const_name: String,
    value_type: ValueType,
    default_raw: String,
    range_raw: Option<(String, String)>,
    step_raw: Option<String>,
    max_len_raw: Option<String>,
    variants: Vec<String>,
    unit: Option<String>,
    persistent: bool,
    apply: String,
    visibility: String,
    help: String,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let config_dir = manifest_dir.join("../../config/cvars");
    println!("cargo::rerun-if-changed={}", config_dir.display());

    let mut files = fs::read_dir(&config_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", config_dir.display()))
        .filter_map(|entry| {
            let path = entry.expect("valid dir entry").path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("toml")).then_some(path)
        })
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty(), "expected at least one TOML file in {}", config_dir.display());

    let mut declarations = Vec::new();
    for path in &files {
        println!("cargo::rerun-if-changed={}", path.display());
        let contents = fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        parse_file(path, &contents, &mut declarations);
    }

    validate_declarations(&declarations);
    let generated = render_generated(&declarations);
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    fs::write(out_dir.join("generated_cvars.rs"), generated).expect("write generated_cvars.rs");
}

fn parse_file(path: &Path, contents: &str, out: &mut Vec<Declaration>) {
    let mut current: Option<(String, Vec<(String, String)>)> = None;
    for (line_index, raw_line) in contents.lines().enumerate() {
        let line_no = line_index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            assert!(
                !header.starts_with('[') && !header.ends_with(']'),
                "{}:{line_no}: expected `[<namespace>.<key>]` table header, got `{line}`",
                path.display()
            );
            if let Some((name, fields)) = current.take() {
                out.push(build_declaration(path, name, fields));
            }
            current = Some((header.trim().to_owned(), Vec::new()));
            continue;
        }
        let Some((_, fields)) = current.as_mut() else {
            panic!("{}:{line_no}: expected `[<namespace>.<key>]` table header before key-value pair", path.display());
        };
        let Some((key, value)) = line.split_once('=') else {
            panic!("{}:{line_no}: expected key = value", path.display());
        };
        let key = key.trim().to_owned();
        let value = value.trim().to_owned();
        if fields.iter().any(|(existing, _)| existing == &key) {
            panic!("{}:{line_no}: duplicate field `{key}`", path.display());
        }
        fields.push((key, value));
    }
    if let Some((name, fields)) = current.take() {
        out.push(build_declaration(path, name, fields));
    }
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && in_quotes {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            in_quotes = !in_quotes;
            continue;
        }
        if byte == b'#' && !in_quotes {
            return &line[..index];
        }
    }
    line
}

fn build_declaration(path: &Path, name: String, fields: Vec<(String, String)>) -> Declaration {
    let mut value_type = None;
    let mut default_raw = None;
    let mut range_raw = None;
    let mut step_raw = None;
    let mut max_len_raw = None;
    let mut variants = Vec::new();
    let mut unit = None;
    let mut persistent = None;
    let mut apply = None;
    let mut visibility = None;
    let mut help = None;

    for (key, raw) in fields {
        match key.as_str() {
            "type" => {
                let kind = parse_string(path, &key, &raw);
                value_type = Some(match kind.as_str() {
                    "bool" => ValueType::Bool,
                    "i64" => ValueType::I64,
                    "f32" => ValueType::F32,
                    "f64" => ValueType::F64,
                    "string" => ValueType::String,
                    "enum" => ValueType::Enum,
                    other => panic!("{}: unsupported cvar type `{other}` on `{name}`", path.display()),
                });
            }
            "default" => default_raw = Some(raw),
            "range" => range_raw = Some(parse_range(path, &name, &raw)),
            "step" => step_raw = Some(raw),
            "max_len" => max_len_raw = Some(raw),
            "values" => variants = parse_string_array(path, &key, &raw),
            "unit" => unit = Some(parse_string(path, &key, &raw)),
            "persistent" => persistent = Some(parse_bool(path, &name, &key, &raw)),
            "apply" => apply = Some(parse_string(path, &key, &raw)),
            "visibility" => visibility = Some(parse_string(path, &key, &raw)),
            "help" => help = Some(parse_string(path, &key, &raw)),
            other => panic!("{}: unknown field `{other}` on `{name}`", path.display()),
        }
    }

    let const_name = derive_const_name(&name);

    Declaration {
        name: name.clone(),
        const_name,
        value_type: value_type.unwrap_or_else(|| panic!("{}: missing `type` on `{name}`", path.display())),
        default_raw: default_raw.unwrap_or_else(|| panic!("{}: missing `default` on `{name}`", path.display())),
        range_raw,
        step_raw,
        max_len_raw,
        variants,
        unit,
        persistent: persistent.unwrap_or_else(|| panic!("{}: missing `persistent` on `{name}`", path.display())),
        apply: apply.unwrap_or_else(|| panic!("{}: missing `apply` on `{name}`", path.display())),
        visibility: visibility.unwrap_or_else(|| "visible".to_owned()),
        help: help.unwrap_or_else(|| panic!("{}: missing `help` on `{name}`", path.display())),
    }
}

fn derive_const_name(name: &str) -> String {
    name.replace('.', "_").to_ascii_uppercase()
}

fn parse_bool(path: &Path, name: &str, key: &str, raw: &str) -> bool {
    match raw {
        "true" => true,
        "false" => false,
        other => panic!("{}: field `{key}` on `{name}` must be a bool, got `{other}`", path.display()),
    }
}

fn parse_range(path: &Path, name: &str, raw: &str) -> (String, String) {
    let Some(inner) = raw.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) else {
        panic!("{}: field `range` on `{name}` must be a `[min, max]` array, got `{raw}`", path.display());
    };
    let parts = inner.split(',').map(str::trim).filter(|part| !part.is_empty()).collect::<Vec<_>>();
    assert_eq!(parts.len(), 2, "{}: field `range` on `{name}` must contain exactly 2 elements, got `{raw}`", path.display());
    (parts[0].to_owned(), parts[1].to_owned())
}

fn parse_string(path: &Path, key: &str, raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        panic!("{}: field `{key}` must be a quoted string, got `{raw}`", path.display());
    };
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

fn parse_string_array(path: &Path, key: &str, raw: &str) -> Vec<String> {
    let Some(inner) = raw.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) else {
        panic!("{}: field `{key}` must be an array of strings", path.display());
    };
    inner.split(',').map(str::trim).filter(|part| !part.is_empty()).map(|part| parse_string(path, key, part)).collect()
}

fn validate_declarations(declarations: &[Declaration]) {
    let mut names = BTreeSet::new();
    let mut const_names = BTreeSet::new();
    for decl in declarations {
        let segments = decl.name.split('.').collect::<Vec<_>>();
        assert!(
            segments.len() >= 2
                && segments.iter().all(
                    |segment| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                ),
            "invalid cvar name `{}`",
            decl.name
        );
        assert!(
            !decl.const_name.is_empty() && decl.const_name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "invalid const_name `{}`",
            decl.const_name
        );
        assert!(names.insert(decl.name.clone()), "duplicate cvar name `{}`", decl.name);
        assert!(const_names.insert(decl.const_name.clone()), "duplicate derived const_name `{}`", decl.const_name);
        assert!(matches!(decl.apply.as_str(), "live" | "restart_required"), "invalid apply `{}`", decl.apply);
        assert!(matches!(decl.visibility.as_str(), "visible" | "hidden"), "invalid visibility `{}`", decl.visibility);
        assert!(!decl.help.trim().is_empty(), "empty help on `{}`", decl.name);

        match decl.value_type {
            ValueType::Bool => {
                assert!(matches!(decl.default_raw.as_str(), "true" | "false"), "invalid bool default on `{}`", decl.name);
                assert!(decl.range_raw.is_none() && decl.step_raw.is_none(), "unexpected numeric bounds on `{}`", decl.name);
            }
            ValueType::I64 => {
                let default: i64 = decl.default_raw.parse().unwrap_or_else(|_| panic!("invalid i64 default on `{}`", decl.name));
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap_or_else(|| panic!("missing range on `{}`", decl.name));
                let min: i64 = min_raw.parse().unwrap_or_else(|_| panic!("invalid i64 min on `{}`", decl.name));
                let max: i64 = max_raw.parse().unwrap_or_else(|_| panic!("invalid i64 max on `{}`", decl.name));
                assert!(min <= max && (min..=max).contains(&default), "i64 bounds violated on `{}`", decl.name);
                assert!(decl.step_raw.is_none(), "unexpected step on i64 cvar `{}`", decl.name);
            }
            ValueType::F32 => {
                let default: f32 = decl.default_raw.parse().unwrap_or_else(|_| panic!("invalid f32 default on `{}`", decl.name));
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap_or_else(|| panic!("missing range on `{}`", decl.name));
                let min: f32 = min_raw.parse().unwrap_or_else(|_| panic!("invalid f32 min on `{}`", decl.name));
                let max: f32 = max_raw.parse().unwrap_or_else(|_| panic!("invalid f32 max on `{}`", decl.name));
                assert!(
                    default.is_finite() && min.is_finite() && max.is_finite() && min <= max && default >= min && default <= max,
                    "f32 bounds violated on `{}`",
                    decl.name
                );
                if let Some(step) = decl.step_raw.as_deref() {
                    let step: f32 = step.parse().unwrap_or_else(|_| panic!("invalid f32 step on `{}`", decl.name));
                    assert!(step.is_finite() && step > 0.0, "f32 step must be positive on `{}`", decl.name);
                }
            }
            ValueType::F64 => {
                let default: f64 = decl.default_raw.parse().unwrap_or_else(|_| panic!("invalid f64 default on `{}`", decl.name));
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap_or_else(|| panic!("missing range on `{}`", decl.name));
                let min: f64 = min_raw.parse().unwrap_or_else(|_| panic!("invalid f64 min on `{}`", decl.name));
                let max: f64 = max_raw.parse().unwrap_or_else(|_| panic!("invalid f64 max on `{}`", decl.name));
                assert!(
                    default.is_finite() && min.is_finite() && max.is_finite() && min <= max && default >= min && default <= max,
                    "f64 bounds violated on `{}`",
                    decl.name
                );
                if let Some(step) = decl.step_raw.as_deref() {
                    let step: f64 = step.parse().unwrap_or_else(|_| panic!("invalid f64 step on `{}`", decl.name));
                    assert!(step.is_finite() && step > 0.0, "f64 step must be positive on `{}`", decl.name);
                }
            }
            ValueType::String => {
                let default = parse_string(Path::new(&decl.name), "default", &decl.default_raw);
                let max_len: usize = decl
                    .max_len_raw
                    .as_deref()
                    .unwrap_or_else(|| panic!("missing max_len on `{}`", decl.name))
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid max_len on `{}`", decl.name));
                assert!(default.len() <= max_len, "string default exceeds max_len on `{}`", decl.name);
            }
            ValueType::Enum => {
                let default = parse_string(Path::new(&decl.name), "default", &decl.default_raw);
                assert!(!decl.variants.is_empty(), "enum cvar `{}` must declare values", decl.name);
                assert!(decl.variants.contains(&default), "enum default `{default}` not in values on `{}`", decl.name);
            }
        }
    }
}

fn render_generated(declarations: &[Declaration]) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "pub const CVAR_COUNT: usize = {};", declarations.len());
    for (index, decl) in declarations.iter().enumerate() {
        let key_type = match decl.value_type {
            ValueType::Bool => "bool",
            ValueType::I64 => "i64",
            ValueType::F32 => "f32",
            ValueType::F64 => "f64",
            ValueType::String => "String",
            ValueType::Enum => "&'static str",
        };
        let _ = writeln!(output, "pub const {}: CvarKey<{key_type}> = CvarKey::new(CvarId({index}));", decl.const_name);
    }

    let _ = writeln!(output, "pub static DESCRIPTORS: [CvarDescriptor; CVAR_COUNT] = [");
    for (index, decl) in declarations.iter().enumerate() {
        let (default_expr, domain_expr) = match decl.value_type {
            ValueType::Bool => (format!("CvarValue::Bool({})", decl.default_raw), "ValueDomain::Bool".to_owned()),
            ValueType::I64 => {
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap();
                (
                    format!("CvarValue::I64({}_i64)", decl.default_raw),
                    format!("ValueDomain::I64 {{ min: {min_raw}_i64, max: {max_raw}_i64 }}"),
                )
            }
            ValueType::F32 => {
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap();
                let step_expr = match decl.step_raw.as_deref() {
                    Some(step) => format!("Some({})", format_f32_literal(step)),
                    None => "None".to_owned(),
                };
                (
                    format!("CvarValue::F32({})", format_f32_literal(&decl.default_raw)),
                    format!(
                        "ValueDomain::F32 {{ min: {}, max: {}, step: {step_expr} }}",
                        format_f32_literal(min_raw),
                        format_f32_literal(max_raw)
                    ),
                )
            }
            ValueType::F64 => {
                let (min_raw, max_raw) = decl.range_raw.as_ref().unwrap();
                let step_expr = match decl.step_raw.as_deref() {
                    Some(step) => format!("Some({})", format_f64_literal(step)),
                    None => "None".to_owned(),
                };
                (
                    format!("CvarValue::F64({})", format_f64_literal(&decl.default_raw)),
                    format!(
                        "ValueDomain::F64 {{ min: {}, max: {}, step: {step_expr} }}",
                        format_f64_literal(min_raw),
                        format_f64_literal(max_raw)
                    ),
                )
            }
            ValueType::String => {
                let default = parse_string(Path::new(&decl.name), "default", &decl.default_raw);
                (
                    format!("CvarValue::String(Cow::Borrowed({default:?}))"),
                    format!("ValueDomain::String {{ max_len: {} }}", decl.max_len_raw.as_deref().unwrap()),
                )
            }
            ValueType::Enum => {
                let default = parse_string(Path::new(&decl.name), "default", &decl.default_raw);
                let variants = decl.variants.iter().map(|variant| format!("{variant:?}")).collect::<Vec<_>>().join(", ");
                (format!("CvarValue::Enum({default:?})"), format!("ValueDomain::Enum {{ variants: &[{variants}] }}"))
            }
        };
        let unit_expr = match decl.unit.as_deref() {
            Some(unit) => format!("Some({unit:?})"),
            None => "None".to_owned(),
        };
        let persistence_expr = if decl.persistent { "PersistenceScope::Global" } else { "PersistenceScope::SessionOnly" };
        let apply_expr = match decl.apply.as_str() {
            "live" => "ApplyScope::Live",
            "restart_required" => "ApplyScope::RestartRequired",
            _ => unreachable!(),
        };
        let visibility_expr = match decl.visibility.as_str() {
            "visible" => "Visibility::Visible",
            "hidden" => "Visibility::Hidden",
            _ => unreachable!(),
        };
        let _ = writeln!(
            output,
            "    CvarDescriptor {{ id: CvarId({index}), name: {:?}, default: {default_expr}, domain: {domain_expr}, unit: {unit_expr}, persistence: {persistence_expr}, apply: {apply_expr}, visibility: {visibility_expr}, help: {:?} }},",
            decl.name, decl.help
        );
    }
    let _ = writeln!(output, "];");
    output
}

fn format_f32_literal(raw: &str) -> String {
    let value: f32 = raw.parse().expect("validated f32");
    if value.fract() == 0.0 { format!("{value:.1}_f32") } else { format!("{value}_f32") }
}

fn format_f64_literal(raw: &str) -> String {
    let value: f64 = raw.parse().expect("validated f64");
    if value.fract() == 0.0 { format!("{value:.1}_f64") } else { format!("{value}_f64") }
}
