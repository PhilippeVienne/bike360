//! Écriture JSON identique à `json.dumps` de Python (fichiers de data/ partagés avec la
//! version Python, empreintes calculées sur du JSON, noms de cache).

use serde_json::Value;

/// `repr(float)` de Python : chiffres les plus courts, notation scientifique hors de 1e-4..1e16.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{:e}", x.abs()); // ex. « 1.2345e-7 », chiffres les plus courts
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let body = if (-4..16).contains(&exp) {
        if exp >= 0 {
            let e = exp as usize;
            let (int, frac) = if digits.len() > e + 1 {
                (digits[..e + 1].to_string(), digits[e + 1..].to_string())
            } else {
                (format!("{digits}{}", "0".repeat(e + 1 - digits.len())), "0".to_string())
            };
            format!("{int}.{frac}")
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        }
    } else {
        let rest = &digits[1..];
        let m = if rest.is_empty() { digits[..1].to_string() } else { format!("{}.{rest}", &digits[..1]) };
        format!("{m}e{}{:02}", if exp < 0 { "-" } else { "+" }, exp.abs())
    };
    format!("{sign}{body}")
}

/// `str(x)` de Python pour une valeur JSON (noms de cache).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => number(n),
        Value::String(s) => s.clone(),
        other => dumps(other),
    }
}

fn number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        let f = n.as_f64().unwrap_or(f64::NAN);
        if f.is_nan() { "nan".into() } else { float_repr(f) }
    }
}

fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    out.push('"');
}

fn write(v: &Value, indent: Option<usize>, sort_keys: bool, level: usize, out: &mut String) {
    let newline = |out: &mut String, level: usize| {
        if let Some(n) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
    };
    let sep = if indent.is_some() { "," } else { ", " };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let s = number(n);
            out.push_str(if s == "nan" { "NaN" } else { &s });
        }
        Value::String(s) => string(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                newline(out, level + 1);
                write(x, indent, sort_keys, level + 1, out);
            }
            newline(out, level);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = m.keys().collect();
            if sort_keys {
                keys.sort();
            }
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                newline(out, level + 1);
                string(k, out);
                out.push_str(": ");
                write(&m[*k], indent, sort_keys, level + 1, out);
            }
            newline(out, level);
            out.push('}');
        }
    }
}

/// `json.dumps(v)`.
pub fn dumps(v: &Value) -> String {
    let mut s = String::new();
    write(v, None, false, 0, &mut s);
    s
}

/// `json.dumps(v, indent=n)`.
pub fn dumps_indent(v: &Value, n: usize) -> String {
    let mut s = String::new();
    write(v, Some(n), false, 0, &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats() {
        for (x, s) in [(1.0, "1.0"), (0.1, "0.1"), (1e-5, "1e-05"), (1e16, "1e+16"), (123.456, "123.456"),
                       (-0.0, "-0.0"), (1234567890123456.0, "1234567890123456.0"), (0.0001, "0.0001"),
                       (1.5e-7, "1.5e-07"), (1782345678.123, "1782345678.123")] {
            assert_eq!(float_repr(x), s);
        }
    }

    #[test]
    fn dumps_like_python() {
        let v = json!({"b": [1, 2.5, "é"], "a": {}, "c": []});
        assert_eq!(dumps(&v), r#"{"b": [1, 2.5, "\u00e9"], "a": {}, "c": []}"#);
        assert_eq!(dumps_indent(&json!([{"x": 1}]), 1), "[\n {\n  \"x\": 1\n }\n]");
    }
}
