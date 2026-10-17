//! The built-in conversions that `nvx.py` applies to the values that `json.loads` returns:
//! `str()`, `int()`, and the `==` with which `in` compares a value with integers.
//!
//! A [`Node`] of a [`Document`] is the value that Python's decoder builds: `None`, `bool`, `int`,
//! `float`, `str`, `list`, or `dict`. [`py_str`] gives `str(value)`, which is the string itself
//! for a string and `repr(value)` for anything else, with floats as Python's `repr` writes them;
//! [`py_int`] gives `int(value)`; and [`equals_int`] gives `value == n`, under which `True`
//! equals 1 and `1.0` equals 1.

use super::builtins::{Int, repr};
use super::json::{Document, Node, NodeId};

/// Why `int(value)` fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntError {
    /// `TypeError` or `ValueError`, which callers of `int` usually catch.
    Invalid,
    /// `OverflowError` for an infinite float, which `nvx.py` does not catch.
    Overflow,
}

/// Returns `int(value)` for the value at `id`: an integer as it is, a float truncated toward
/// zero, `True` and `False` as 1 and 0, and a string as `int(text)` reads it.
pub(crate) fn py_int(document: &Document, id: NodeId) -> Result<Int, IntError> {
    match document.node(id) {
        Node::Integer(literal) => Int::parse(literal).map_err(|_| IntError::Invalid),
        Node::Bool(value) => Ok(Int::from(u64::from(*value))),
        Node::Float(value) if value.is_nan() => Err(IntError::Invalid),
        Node::Float(value) if value.is_infinite() => Err(IntError::Overflow),
        Node::Float(value) => {
            Int::parse(&format!("{:.0}", value.trunc())).map_err(|_| IntError::Invalid)
        }
        Node::String(text) => text
            .as_str()
            .and_then(|text| Int::parse(text).ok())
            .ok_or(IntError::Invalid),
        Node::Null | Node::Array(_) | Node::Object(_) => Err(IntError::Invalid),
    }
}

/// Returns `str(value)` for the value at `id`, or `None` for a string that is not valid
/// Unicode, which holds unpaired surrogates.
pub(crate) fn py_str(document: &Document, id: NodeId) -> Option<String> {
    match document.node(id) {
        Node::String(text) => text.as_str().map(str::to_owned),
        _ => py_repr(document, id),
    }
}

/// Returns `repr(value)` for the value at `id`, or `None` if a string in it is not valid
/// Unicode.
pub(crate) fn py_repr(document: &Document, id: NodeId) -> Option<String> {
    Some(match document.node(id) {
        Node::Null => "None".to_owned(),
        Node::Bool(true) => "True".to_owned(),
        Node::Bool(false) => "False".to_owned(),
        Node::Integer(literal) => Int::parse(literal).ok()?.to_string(),
        Node::Float(value) => float_repr(*value),
        Node::String(text) => repr(text.as_str()?),
        Node::Array(items) => {
            let items: Option<Vec<String>> =
                items.iter().map(|&item| py_repr(document, item)).collect();
            format!("[{}]", items?.join(", "))
        }
        Node::Object(members) => {
            // A dict keeps the first position of a repeated name, with its last value.
            let mut entries: Vec<(&[u8], String, NodeId)> = Vec::new();
            for (name, value) in members {
                match entries.iter_mut().find(|entry| entry.0 == name.as_bytes()) {
                    Some(entry) => entry.2 = *value,
                    None => entries.push((name.as_bytes(), repr(name.as_str()?), *value)),
                }
            }
            let entries: Option<Vec<String>> = entries
                .iter()
                .map(|(_, name, value)| Some(format!("{name}: {}", py_repr(document, *value)?)))
                .collect();
            format!("{{{}}}", entries?.join(", "))
        }
    })
}

/// Returns whether the value at `id` equals the integer `expected` under Python's `==`.
pub(crate) fn equals_int(document: &Document, id: NodeId, expected: u64) -> bool {
    match document.node(id) {
        Node::Integer(literal) => Int::parse(literal).ok() == Some(Int::from(expected)),
        Node::Bool(value) => u64::from(*value) == expected,
        // Every integer that NVX compares with is exact as a float.
        Node::Float(value) => *value == expected as f64,
        _ => false,
    }
}

/// Returns `repr(value)` for a float: the shortest digits that read back as `value`, in fixed
/// notation from 1e-4 up to 1e16 with at least one digit after the point, and in exponent
/// notation otherwise, with a sign and at least two exponent digits.
pub(crate) fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let (digits, exponent) = shortest_digits(value.abs());
    // The position of the decimal point relative to the start of the digits.
    let point = exponent + 1;
    let length = digits.len() as i32;
    let body = if (-3..=16).contains(&point) {
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point >= length {
            format!("{digits}{}.0", "0".repeat((point - length) as usize))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{whole}.{fraction}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exponent = point - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{fraction}e{exponent_sign}{:02}", exponent.abs())
    };
    format!("{sign}{body}")
}

/// Returns the shortest digits that read back as `value`, a finite float that is not negative,
/// and the decimal exponent of the first of them, as `repr` chooses them.
///
/// Rust's `{:e}` writes the shortest digits that read back as the value. Of two such digit
/// strings that are equally close to the value, `repr` takes the one that ends in an even digit,
/// as `_Py_dg_dtoa` rounds, but `{:e}` may take the other: for 2⁻²⁵, `repr` gives
/// `2.9802322387695312e-08`, where `{:e}` gives `2.9802322387695313e-8`.
fn shortest_digits(value: f64) -> (String, i32) {
    let (digits, exponent) = scientific_digits(&format!("{value:e}"));
    if !digits.ends_with(['1', '3', '5', '7', '9']) {
        return (digits, exponent);
    }
    // The decimal expansion of a float ends within 767 significant digits, so this one is exact.
    let (exact, exact_exponent) = scientific_digits(&format!("{value:.767e}"));
    if exact_exponent != exponent {
        return (digits, exponent);
    }
    let exact = exact.trim_end_matches('0');
    let (stem, last) = digits.split_at(digits.len() - 1);
    let last = last.as_bytes()[0];
    let below = format!("{stem}{}", char::from(last - 1));
    let even = if exact == format!("{digits}5") && last != b'9' {
        // Halfway between the digits and those just above them.
        format!("{stem}{}", char::from(last + 1))
    } else if exact == format!("{below}5") {
        // Halfway between the digits and those just below them.
        below
    } else {
        return (digits, exponent);
    };
    let reads_back = format!("0.{even}e{}", exponent + 1).parse::<f64>() == Ok(value);
    if even.starts_with('0') || !reads_back {
        return (digits, exponent);
    }
    (even, exponent)
}

/// Splits what `{:e}` writes for a float that is not negative into its digits, without the
/// point, and its decimal exponent.
fn scientific_digits(scientific: &str) -> (String, i32) {
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` writes an exponent");
    let digits = mantissa.chars().filter(|&c| c != '.').collect();
    let exponent = exponent.parse().expect("`{:e}` writes a decimal exponent");
    (digits, exponent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::python::json::{Loads, read};

    fn document(text: &str) -> Document {
        read(text, &mut Loads).unwrap()
    }

    #[test]
    fn floats_print_as_python_repr() {
        for (value, expected) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (-0.0, "-0.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (2.5, "2.5"),
            (1e300, "1e+300"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (12345.678, "12345.678"),
            // Halfway between two shortest digit strings, `repr` takes the one that ends in an
            // even digit.
            (2f64.powi(-25), "2.9802322387695312e-08"),
            (f64::from_bits(0x430e_1c6d_958d_7b72), "1059438285926254.2"),
            (f64::from_bits(0xc314_41f2_33f6_3165), "-1425502010969177.2"),
            (f64::from_bits(0x42b7_fa57_c450_e950), "26363981746409.312"),
            (f64::from_bits(0xc2d8_c0fe_9119_c088), "-108868734838530.12"),
        ] {
            assert_eq!(float_repr(value), expected, "{value:?}");
        }
    }

    #[test]
    fn values_convert_as_python_converts_them() {
        let value = document(
            r#"[1, -2.9, true, false, "7", " 8 ", "x", null, [1], {"a": 1}, 1e400, NaN, 1.0]"#,
        );
        let Node::Array(items) = document_root(&value) else {
            panic!("not an array")
        };
        let ints: Vec<Result<String, IntError>> = items
            .iter()
            .map(|&item| py_int(&value, item).map(|value| value.to_string()))
            .collect();
        let ok = |text: &str| Ok(text.to_owned());
        assert_eq!(
            ints,
            [
                ok("1"),
                ok("-2"),
                ok("1"),
                ok("0"),
                ok("7"),
                ok("8"),
                Err(IntError::Invalid),
                Err(IntError::Invalid),
                Err(IntError::Invalid),
                Err(IntError::Invalid),
                Err(IntError::Overflow),
                Err(IntError::Invalid),
                ok("1"),
            ]
        );
        let strings: Vec<String> = items
            .iter()
            .map(|&item| py_str(&value, item).unwrap())
            .collect();
        assert_eq!(
            strings,
            [
                "1", "-2.9", "True", "False", "7", " 8 ", "x", "None", "[1]", "{'a': 1}", "inf",
                "nan", "1.0"
            ]
        );
        let equal: Vec<bool> = items
            .iter()
            .map(|&item| equals_int(&value, item, 1))
            .collect();
        assert_eq!(
            equal,
            [
                true, false, true, false, false, false, false, false, false, false, false, false,
                true
            ]
        );
    }

    #[test]
    fn containers_print_as_python_repr() {
        let value = document(r#"{"b": ["it's", 2.5, {"x": null}], "a": 1, "b": "last"}"#);
        assert_eq!(
            py_str(&value, value.root()).unwrap(),
            "{'b': 'last', 'a': 1}"
        );
        let value = document(r#"["it's", [2.5, {"x": null}]]"#);
        assert_eq!(
            py_str(&value, value.root()).unwrap(),
            "[\"it's\", [2.5, {'x': None}]]"
        );
        assert_eq!(py_str(&document(r#""\ud800""#), 0), None);
    }

    fn document_root(document: &Document) -> &Node {
        document.node(document.root())
    }
}
